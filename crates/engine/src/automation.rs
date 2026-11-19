//! "When X, do Y" (OBS's Advanced Scene Switcher and macro plugins, built in, no code): a rule
//! has one *when* (a follow, a raid, a sub, support, a channel reward, a chat command, the stream
//! starting or ending, a scene going on air, every few minutes while live) and up to ten steps
//! (switch scene, show or hide a source, a filter on or off, save the replay, record, a chat
//! message, wait). Kept in the data folder's `automations.json`.
//!
//! Safety: a rule runs at most once per its cooldown (3 seconds at least), so chat can't make it
//! run in a loop; a chat command only answers mods and the streamer unless the rule says anyone;
//! steps run one after another off the event pipeline, which never waits for them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ferrocast_core::event::{Event, EventKind, Origin, PaymentStatus, Roles};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::studio_host::{FilterEdit, ItemEdit, StudioAct};
use crate::{Engine, EngineError};

pub const MAX_RULES: usize = 50;
pub const MAX_STEPS: usize = 10;
const MAX_TEXT: usize = 300;
const MAX_NAME: usize = 60;
const MAX_WAIT_S: u32 = 600;

/// What makes a rule run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum When {
    Follow,
    /// A raid of at least `min` viewers.
    Raid {
        #[serde(default)]
        min: u32,
    },
    /// A sub, a resub or gifted subs.
    Sub,
    /// A tip, a paid message (Super Chat) or a platform gift (bits, stars).
    Support,
    /// A channel reward by its name (any, empty).
    Reward {
        #[serde(default)]
        name: String,
    },
    /// A chat message that starts with `text` ("!clip"); mods and the streamer only, unless `anyone`.
    Chat {
        text: String,
        #[serde(default)]
        anyone: bool,
    },
    LiveStart,
    LiveEnd,
    /// A scene going on air.
    Scene {
        name: String,
    },
    /// Every so many minutes while live.
    Every {
        minutes: u32,
    },
}

/// One step of a rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Scene {
        name: String,
    },
    Show {
        scene: String,
        source: String,
    },
    Hide {
        scene: String,
        source: String,
    },
    Filter {
        source: String,
        filter: String,
        on: bool,
    },
    SaveReplay,
    Record {
        on: bool,
    },
    /// A chat message; `{name}` is who set it off.
    Chat {
        text: String,
    },
    Wait {
        seconds: u32,
    },
}

fn yes() -> bool {
    true
}
fn ten() -> u32 {
    10
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub on: bool,
    pub when: When,
    #[serde(default)]
    pub then: Vec<Step>,
    /// Seconds before it can run again (3 to 3600).
    #[serde(default = "ten")]
    pub cooldown_s: u32,
}

/// The rules, and when each last ran (its cooldown) or last fired by the clock.
#[derive(Debug, Default)]
pub(crate) struct Automations {
    rules: Vec<Rule>,
    loaded: bool,
    last: HashMap<String, Instant>,
    scene: Option<String>,
}

fn bad(why: String) -> EngineError {
    EngineError::Refused {
        status: 422,
        code: "automation_invalid",
        message: why,
        hint: "Fix the rule and save again.".into(),
        retry_after: None,
    }
}

fn text_ok(what: &str, s: &str, empty_ok: bool) -> Result<(), EngineError> {
    if !empty_ok && s.trim().is_empty() {
        return Err(bad(format!("{what} is empty.")));
    }
    if s.chars().count() > MAX_TEXT {
        return Err(bad(format!("{what} is longer than {MAX_TEXT} characters.")));
    }
    Ok(())
}

/// Checks a list of rules before it's saved.
pub fn check(rules: &[Rule]) -> Result<(), EngineError> {
    if rules.len() > MAX_RULES {
        return Err(bad(format!("Up to {MAX_RULES} rules.")));
    }
    let mut ids = std::collections::HashSet::new();
    for r in rules {
        let id_ok = !r.id.is_empty()
            && r.id.len() <= 64
            && r.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(bad(format!("The rule id \"{}\" isn't one (letters, digits, - and _).", r.id)));
        }
        if !ids.insert(r.id.as_str()) {
            return Err(bad(format!("Two rules have the id \"{}\".", r.id)));
        }
        let name = r.name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME {
            return Err(bad(format!("A rule's name is empty or longer than {MAX_NAME} characters.")));
        }
        if !(3..=3600).contains(&r.cooldown_s) {
            return Err(bad(format!("\"{name}\": wait 3 to 3600 seconds between runs.")));
        }
        match &r.when {
            When::Chat { text, .. } => text_ok(&format!("\"{name}\"'s chat command"), text, false)?,
            When::Reward { name: n } => text_ok(&format!("\"{name}\"'s reward"), n, true)?,
            When::Scene { name: n } => text_ok(&format!("\"{name}\"'s scene"), n, false)?,
            When::Every { minutes } if !(1..=720).contains(minutes) => {
                return Err(bad(format!("\"{name}\": every 1 to 720 minutes.")));
            }
            _ => {}
        }
        if r.then.is_empty() {
            return Err(bad(format!("\"{name}\" has nothing to do: add a step.")));
        }
        if r.then.len() > MAX_STEPS {
            return Err(bad(format!("\"{name}\": up to {MAX_STEPS} steps.")));
        }
        for s in &r.then {
            match s {
                Step::Scene { name: n } => text_ok("A scene", n, false)?,
                Step::Show { scene, source } | Step::Hide { scene, source } => {
                    text_ok("A scene", scene, false)?;
                    text_ok("A source", source, false)?;
                }
                Step::Filter { source, filter, .. } => {
                    text_ok("A source", source, false)?;
                    text_ok("A filter", filter, false)?;
                }
                Step::Chat { text } => text_ok("A chat message", text, false)?,
                Step::Wait { seconds } if !(1..=MAX_WAIT_S).contains(seconds) => {
                    return Err(bad(format!("\"{name}\": wait 1 to {MAX_WAIT_S} seconds.")));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Whether `e` sets off `w`.
pub fn sets_off(w: &When, e: &Event) -> bool {
    match (w, &e.kind) {
        (When::Follow, EventKind::Follow { .. }) => true,
        (When::Raid { min }, EventKind::Raid { viewers, .. }) => viewers >= min,
        (When::Sub, EventKind::Subscription { .. } | EventKind::GiftBatch { .. }) => true,
        (When::Support, EventKind::Tip { status, .. }) => *status == PaymentStatus::Captured,
        (When::Support, EventKind::PaidMessage { .. } | EventKind::PlatformGift { .. }) => true,
        (When::Reward { name }, EventKind::RewardRedemption { reward, .. }) => {
            name.trim().is_empty() || reward.trim().eq_ignore_ascii_case(name.trim())
        }
        (When::Chat { text, anyone }, EventKind::ChatMessage { actor, content, .. }) => {
            let said = content.text.trim_start();
            let cmd = text.trim();
            let starts = said.len() >= cmd.len()
                && said.is_char_boundary(cmd.len())
                && said[..cmd.len()].eq_ignore_ascii_case(cmd)
                // "!clip" is set off by "!clip" and "!clip now", not "!clipboard".
                && said[cmd.len()..].chars().next().is_none_or(char::is_whitespace);
            let may = *anyone || actor.roles.contains(Roles::BROADCASTER) || actor.roles.contains(Roles::MODERATOR);
            !cmd.is_empty() && starts && may
        }
        (When::LiveStart, EventKind::StreamOnline { .. }) => true,
        (When::LiveEnd, EventKind::StreamOffline) => true,
        _ => false,
    }
}

/// `{name}` in a chat step, filled in.
fn fill(text: &str, who: &str) -> String {
    text.replace("{name}", who)
}

impl Engine {
    fn automations_file(&self) -> std::path::PathBuf {
        self.store().dir().join("automations.json")
    }

    fn with_automations<T>(&self, f: impl FnOnce(&mut Automations) -> T) -> T {
        let mut a = self.inner.automations.lock().unwrap_or_else(|e| e.into_inner());
        if !a.loaded {
            a.loaded = true;
            a.rules = std::fs::read(self.automations_file())
                .ok()
                .and_then(|b| serde_json::from_slice::<Vec<Rule>>(&b).ok())
                .filter(|r| check(r).is_ok())
                .unwrap_or_default();
        }
        f(&mut a)
    }

    pub fn automations(&self) -> Vec<Rule> {
        self.with_automations(|a| a.rules.clone())
    }

    /// Saves the whole list (checked first).
    pub fn set_automations(&self, rules: Vec<Rule>) -> Result<Vec<Rule>, EngineError> {
        check(&rules)?;
        let body = serde_json::to_vec_pretty(&rules).map_err(|e| EngineError::Internal(e.to_string()))?;
        let file = self.automations_file();
        let tmp = file.with_extension("json.new");
        // Under the lock, so two saves can't write the same file at once or end up apart.
        self.with_automations(|a| {
            std::fs::write(&tmp, body)
                .and_then(|()| std::fs::rename(&tmp, &file))
                .map_err(|e| EngineError::Internal(format!("The rules couldn't be saved: {e}")))?;
            a.last.retain(|id, _| rules.iter().any(|r| &r.id == id));
            a.rules = rules.clone();
            Ok::<(), EngineError>(())
        })?;
        Ok(rules)
    }

    /// Runs a rule now (its Test button), cooldown or not.
    pub fn run_automation(&self, id: &str) -> Result<(), EngineError> {
        let rule =
            self.with_automations(|a| a.rules.iter().find(|r| r.id == id).cloned()).ok_or(EngineError::Missing {
                code: "automation_not_found",
                message: "There's no such rule.",
                hint: "Refresh the list of rules.",
            })?;
        self.spawn_steps(rule, "you".into());
        Ok(())
    }

    /// The rules that `e` sets off run (live events and tests; never old ones caught up on).
    pub(crate) fn automation_saw(&self, e: &Event) {
        if !matches!(e.origin, Origin::Live | Origin::Test) {
            return;
        }
        let who = e.kind.actor().map(|a| a.display_name.clone()).unwrap_or_default();
        let due = self.with_automations(|a| {
            let now = Instant::now();
            let mut due = Vec::new();
            for r in a.rules.iter().filter(|r| r.on && sets_off(&r.when, e)) {
                if a.last.get(&r.id).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(r.cooldown_s.into()))
                {
                    continue;
                }
                a.last.insert(r.id.clone(), now);
                due.push(r.clone());
            }
            due
        });
        for r in due {
            self.spawn_steps(r, who.clone());
        }
    }

    /// Once a second: a scene that went on air, and rules due by the clock while live.
    pub(crate) async fn automation_tick(&self) {
        let (wants_scene, every): (bool, Vec<Rule>) = self.with_automations(|a| {
            let on = a.rules.iter().filter(|r| r.on);
            (
                on.clone().any(|r| matches!(r.when, When::Scene { .. })),
                on.filter(|r| matches!(r.when, When::Every { .. })).cloned().collect(),
            )
        });
        if wants_scene && let Ok(v) = self.studio_act(StudioAct::Scenes).await {
            let now = v.get("current").and_then(Value::as_str).map(String::from);
            let due = self.with_automations(|a| {
                let changed = a.scene.is_some() && now.is_some() && a.scene != now;
                a.scene = now.clone();
                if !changed {
                    return Vec::new();
                }
                let at = Instant::now();
                let mut due = Vec::new();
                for r in a.rules.iter().filter(|r| r.on) {
                    let When::Scene { name } = &r.when else { continue };
                    if now.as_deref() != Some(name.as_str())
                        || a.last
                            .get(&r.id)
                            .is_some_and(|t| at.duration_since(*t) < Duration::from_secs(r.cooldown_s.into()))
                    {
                        continue;
                    }
                    a.last.insert(r.id.clone(), at);
                    due.push(r.clone());
                }
                due
            });
            for r in due {
                self.spawn_steps(r, String::new());
            }
        }
        if every.is_empty() {
            return;
        }
        let live = self.state().live.values().any(|l| *l);
        let due = self.with_automations(|a| {
            let at = Instant::now();
            let mut due = Vec::new();
            for r in &every {
                let When::Every { minutes } = r.when else { continue };
                if !live {
                    // The clock starts again when the stream does.
                    a.last.remove(&r.id);
                    continue;
                }
                match a.last.get(&r.id) {
                    None => {
                        a.last.insert(r.id.clone(), at);
                    }
                    Some(t) if at.duration_since(*t) >= Duration::from_secs(u64::from(minutes) * 60) => {
                        a.last.insert(r.id.clone(), at);
                        due.push(r.clone());
                    }
                    Some(_) => {}
                }
            }
            due
        });
        for r in due {
            self.spawn_steps(r, String::new());
        }
    }

    fn spawn_steps(&self, rule: Rule, who: String) {
        let engine = self.clone();
        tokio::spawn(async move {
            for (i, step) in rule.then.iter().enumerate() {
                if let Err(e) = engine.automation_step(step, &who).await {
                    tracing::warn!(rule = %rule.name, step = i + 1, "an automation step failed: {e}");
                    engine.send_live(LiveFrame::new(
                        "notice",
                        &Notice {
                            key: Some(format!("automation:{}", rule.id)),
                            level: NoticeLevel::Warn,
                            message: format!("\"{}\" stopped at step {}: {e}", rule.name, i + 1),
                            action: Some(crate::frames::NoticeAction {
                                label: "Edit".into(),
                                href: "#/settings/automations".into(),
                            }),
                        },
                    ));
                    return;
                }
            }
            tracing::info!(rule = %rule.name, "an automation ran");
        });
    }

    async fn item_id(&self, scene: &str, source: &str) -> Result<i64, EngineError> {
        let items = self.studio_act(StudioAct::Items { scene: scene.into() }).await?;
        items
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i.get("name").and_then(Value::as_str) == Some(source))
            .and_then(|i| i.get("id").and_then(Value::as_i64))
            .ok_or_else(|| EngineError::Invalid(format!("\"{source}\" isn't in the scene \"{scene}\".")))
    }

    pub(crate) async fn automation_step(&self, step: &Step, who: &str) -> Result<(), EngineError> {
        match step {
            Step::Scene { name } => self.studio_act(StudioAct::SetCurrent { scene: name.clone() }).await.map(drop),
            Step::Show { scene, source } | Step::Hide { scene, source } => {
                let id = self.item_id(scene, source).await?;
                let edit = if matches!(step, Step::Show { .. }) { ItemEdit::Show } else { ItemEdit::Hide };
                self.studio_act(StudioAct::EditItem { scene: scene.clone(), id, edit }).await.map(drop)
            }
            Step::Filter { source, filter, on } => {
                let edit = if *on { FilterEdit::Enable } else { FilterEdit::Disable };
                self.studio_act(StudioAct::EditFilter { name: source.clone(), filter: filter.clone(), edit })
                    .await
                    .map(drop)
            }
            Step::SaveReplay => self.studio_replay_save().await.map(drop),
            Step::Record { on } => self.studio_record(*on).await.map(drop),
            Step::Chat { text } => {
                let out = self.send_chat(&fill(text, who), &[], None).await?;
                if out.ok.is_empty() {
                    let why = out.failed.first().map_or("no platform is connected".to_string(), |f| f.reason.clone());
                    return Err(EngineError::Unavailable(format!("The chat message didn't go out: {why}.")));
                }
                Ok(())
            }
            Step::Wait { seconds } => {
                tokio::time::sleep(Duration::from_secs(u64::from((*seconds).min(MAX_WAIT_S)))).await;
                Ok(())
            }
        }
    }
}

/// Every second, the clock and scene rules.
pub(crate) async fn run(engine: Engine) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        engine.automation_tick().await;
    }
}

/// What a new rule looks like, for the API's docs and tests.
pub fn example() -> Value {
    json!({
        "id": "clip", "name": "Clip it", "on": true, "cooldown_s": 30,
        "when": { "type": "chat", "text": "!clip", "anyone": false },
        "then": [{ "type": "save_replay" }, { "type": "chat", "text": "Saved, {name}!" }],
    })
}

#[cfg(test)]
pub(crate) mod tests;
