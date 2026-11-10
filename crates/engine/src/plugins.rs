//! Plugins (OBS has them; Ferrocast's are safe by design, OBS-COMPETE.md §4): a `.zip` with a
//! `plugin.json` and what it brings. The first kind is an **effect**: a WGSL `effect` function
//! that runs on the graphics card with only the picture and its sliders, so it can't read
//! files, keys or the network; it's checked to compile first, and it may not use `loop` or
//! `while` (a fixed `for` is fine), so a plugin can't keep the graphics card busy forever. Installed
//! under the data folder's `plugins/<id>/`; which ones are off is kept in `plugins/off.json`.
//!
//! `plugin.json`:
//! ```json
//! { "id": "com.example.crt", "name": "Retro CRT", "version": "1.0.0", "author": "Ana",
//!   "description": "Scanlines and glow.", "kind": "effect", "effect": "effect.wgsl" }
//! ```

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Engine, EngineError};

/// The biggest plugin file taken.
pub const MAX_ZIP: u64 = 2 * 1024 * 1024;
/// The biggest single file inside one, and how many files it may hold.
const MAX_ENTRY: u64 = 512 * 1024;
const MAX_ENTRIES: usize = 20;
/// What a plugin may hold: its manifest, its effects, words and a picture; a logic plugin its
/// WebAssembly module, a panel its page.
const ALLOWED: &[&str] = &["json", "wgsl", "md", "txt", "png", "wasm", "html"];
/// What each kind may hold beyond the common files.
fn kind_files(kind: &str) -> &'static [&'static str] {
    match kind {
        "logic" => &["wasm"],
        "panel" => &["html"],
        _ => &["wgsl"],
    }
}
/// The steps a plugin may ask to be allowed (as "When X, do Y" has them; not waiting).
pub const ACTIONS: &[&str] = &["scene", "show", "hide", "filter", "save_replay", "record", "chat"];

/// A plugin's `plugin.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    /// What it is: "effect" (a WGSL effect), "logic" (a WebAssembly module told about events)
    /// or "panel" (a page shown in the dashboard, in a sandbox).
    pub kind: String,
    /// An effect's file, beside `plugin.json`.
    #[serde(default)]
    pub effect: String,
    /// A logic plugin's module (`.wasm`).
    #[serde(default)]
    pub module: String,
    /// A panel's page (`.html`, everything inline).
    #[serde(default)]
    pub panel: String,
    /// The steps a logic plugin or panel may ask for (`ACTIONS`), shown at install.
    #[serde(default)]
    pub actions: Vec<String>,
}

/// An installed plugin, as the app lists it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub kind: String,
    /// What it may do (a logic plugin's or panel's steps).
    pub actions: Vec<String>,
    pub on: bool,
    /// Why it can't run now (its effect no longer compiles…), or null.
    pub problem: Option<String>,
}

/// A refusal with its status, words and what to do.
fn refused(status: u16, code: &'static str, message: impl Into<String>, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

/// An id: 3 to 64 of a-z, 0-9, `.`, `-`, `_`, starting with a letter (a reverse domain works).
fn good_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && !id.contains("..")
}

/// Checks a manifest's fields → what's wrong, in words.
pub fn check_manifest(m: &Manifest) -> Result<(), String> {
    if !good_id(&m.id) {
        return Err(format!(
            "its id \"{}\" isn't one: 3 to 64 small letters, digits, dots, dashes, starting with a letter",
            m.id
        ));
    }
    let len = |s: &str, max: usize| s.chars().count() <= max;
    if m.name.trim().is_empty() || !len(&m.name, 60) {
        return Err("its name is empty or longer than 60 characters".into());
    }
    if m.version.trim().is_empty() || !len(&m.version, 20) {
        return Err("its version is empty or longer than 20 characters".into());
    }
    if !len(&m.author, 60) || !len(&m.description, 200) {
        return Err("its author (60) or description (200) is too long".into());
    }
    let beside =
        |f: &str, ext: &str| Path::new(f).extension().and_then(|e| e.to_str()) == Some(ext) && !f.contains(['/', '\\']);
    match m.kind.as_str() {
        "effect" if !beside(&m.effect, "wgsl") => Err("its effect must be a .wgsl file beside plugin.json".into()),
        "logic" if !beside(&m.module, "wasm") => Err("its module must be a .wasm file beside plugin.json".into()),
        "panel" if !beside(&m.panel, "html") => Err("its panel must be an .html file beside plugin.json".into()),
        "effect" | "logic" | "panel" => {
            if m.kind == "effect" && !m.actions.is_empty() {
                return Err("an effect can't ask for actions".into());
            }
            if let Some(a) = m.actions.iter().find(|a| !ACTIONS.contains(&a.as_str())) {
                return Err(format!("it asks for \"{a}\", which isn't an action plugins can have"));
            }
            Ok(())
        }
        other => Err(format!("it's a \"{other}\" plugin; this Ferrocast takes effect, logic and panel plugins")),
    }
}

/// The file that is the plugin's work: its effect, module or panel.
fn main_file(m: &Manifest) -> &str {
    match m.kind.as_str() {
        "logic" => &m.module,
        "panel" => &m.panel,
        _ => &m.effect,
    }
}

/// A zip entry's name, inside the plugin (one leading folder allowed and dropped) → None when
/// it climbs out, is absolute, or is a folder.
fn inner_name(raw: &str) -> Option<String> {
    let path = Path::new(raw);
    if path.components().any(|c| !matches!(c, Component::Normal(_))) || raw.ends_with('/') {
        return None;
    }
    let parts: Vec<&str> = raw.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [file] => Some((*file).to_string()),
        [_folder, file] => Some((*file).to_string()),
        _ => None,
    }
}

/// A plugin's files: name (beside `plugin.json`) and bytes.
pub type Files = Vec<(String, Vec<u8>)>;

/// Reads a plugin file: its manifest and every file in it, all checked.
pub fn read_zip(path: &Path) -> Result<(Manifest, Files), EngineError> {
    let not_one = |why: &str| {
        refused(
            415,
            "plugin_not_a_plugin",
            format!("That isn't a Ferrocast plugin: {why}."),
            "Pick the plugin's .zip file.",
        )
    };
    let size = std::fs::metadata(path).map_err(|e| EngineError::Internal(e.to_string()))?.len();
    if size > MAX_ZIP {
        return Err(refused(
            413,
            "plugin_too_big",
            format!("The plugin is {} KB; plugins are up to {} KB.", size / 1024, MAX_ZIP / 1024),
            "Ask its maker for a smaller one.",
        ));
    }
    let file = std::fs::File::open(path).map_err(|e| EngineError::Internal(e.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| not_one("it isn't a zip file"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(not_one(&format!("it holds {} files; a plugin holds up to {MAX_ENTRIES}", zip.len())));
    }
    let mut files: Files = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|_| not_one("a file in it can't be read"))?;
        if entry.is_dir() {
            continue;
        }
        let raw = entry.name().map_err(|_| not_one("a file in it has an unreadable name"))?.to_string();
        let name = inner_name(&raw).ok_or_else(|| not_one(&format!("\"{raw}\" is outside the plugin's folder")))?;
        let ext = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !ALLOWED.contains(&ext.as_str()) {
            return Err(refused(
                415,
                "plugin_file_type",
                format!(
                    "The plugin holds \"{name}\"; plugins hold only .json, .wgsl, .wasm, .html, .md, .txt and .png files."
                ),
                "Plugins can't bring programs. Ask its maker for an effect plugin.",
            ));
        }
        if entry.size() > MAX_ENTRY {
            return Err(refused(
                413,
                "plugin_too_big",
                format!("\"{name}\" in the plugin is bigger than {} KB.", MAX_ENTRY / 1024),
                "Ask its maker for a smaller one.",
            ));
        }
        let mut bytes = Vec::new();
        // Read no more than the limit, whatever the zip claims.
        (&mut entry).take(MAX_ENTRY + 1).read_to_end(&mut bytes).map_err(|_| not_one("a file in it is broken"))?;
        if bytes.len() as u64 > MAX_ENTRY {
            return Err(refused(413, "plugin_too_big", format!("\"{name}\" in the plugin is too big."), ""));
        }
        if files.iter().any(|(n, _)| *n == name) {
            return Err(not_one(&format!("it holds \"{name}\" twice")));
        }
        files.push((name, bytes));
    }
    let manifest =
        files.iter().find(|(n, _)| n == "plugin.json").ok_or_else(|| not_one("it has no plugin.json")).and_then(
            |(_, b)| {
                serde_json::from_slice::<Manifest>(b).map_err(|e| not_one(&format!("its plugin.json is wrong ({e})")))
            },
        )?;
    check_manifest(&manifest).map_err(|why| {
        refused(
            422,
            "plugin_manifest",
            format!("The plugin's plugin.json is wrong: {why}."),
            "Ask its maker to fix it.",
        )
    })?;
    // Only the files its kind uses: a module or a page in an effect plugin is refused.
    for (name, _) in &files {
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let common = ["json", "md", "txt", "png"].contains(&ext.as_str());
        if !common && !kind_files(&manifest.kind).contains(&ext.as_str()) {
            return Err(refused(
                415,
                "plugin_file_type",
                format!("The plugin holds \"{name}\", which a {} plugin doesn't use.", manifest.kind),
                "Ask its maker for a plugin without it.",
            ));
        }
    }
    let main = main_file(&manifest);
    let body = files
        .iter()
        .find(|(n, _)| n == main)
        .ok_or_else(|| not_one(&format!("its {} \"{main}\" isn't in it", manifest.kind)))?;
    match manifest.kind.as_str() {
        "logic" => {
            crate::plugin_logic::load(&body.1).map_err(|why| {
                refused(
                    422,
                    "plugin_module_broken",
                    format!("The plugin's module doesn't work: {why}."),
                    "Ask its maker for a version that does.",
                )
            })?;
            return Ok((manifest, files));
        }
        "panel" => {
            std::str::from_utf8(&body.1).map_err(|_| not_one("its panel isn't text"))?;
            return Ok((manifest, files));
        }
        _ => {}
    }
    let code = std::str::from_utf8(&body.1).map_err(|_| not_one("its effect isn't text"))?;
    if let Some(word) = endless(code) {
        return Err(refused(
            422,
            "plugin_effect_broken",
            format!("The plugin's effect uses `{word}`; plugin effects use only `for` loops with a fixed count."),
            "Ask its maker for a version without it.",
        ));
    }
    ferrocore::shaderfx::check(code).map_err(|why| {
        refused(
            422,
            "plugin_effect_broken",
            format!("The plugin's effect doesn't work: {why}."),
            "Ask its maker for a version that does.",
        )
    })?;
    Ok((manifest, files))
}

/// A `loop` or `while` in an effect's code (outside comments), which could run without end.
fn endless(code: &str) -> Option<&'static str> {
    let text: String = code.lines().map(|l| l.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
    let words: Vec<&str> = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).collect();
    ["loop", "while"].into_iter().find(|w| words.contains(w))
}

/// Which plugins are off (`off.json`).
fn off_list(dir: &Path) -> Vec<String> {
    std::fs::read(dir.join("off.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_off(dir: &Path, off: &[String]) -> Result<(), EngineError> {
    let body = serde_json::to_vec(off).map_err(|e| EngineError::Internal(e.to_string()))?;
    std::fs::write(dir.join("off.json"), body).map_err(|e| EngineError::Internal(e.to_string()))
}

/// The installed plugins, by name.
pub fn list(dir: &Path) -> Vec<PluginInfo> {
    let off = off_list(dir);
    let mut out: Vec<PluginInfo> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let folder = e.path();
            let m: Manifest = serde_json::from_slice(&std::fs::read(folder.join("plugin.json")).ok()?).ok()?;
            // Only a folder named for its id (not an install's work folders, nor one renamed by
            // hand: it couldn't be turned off or removed by its id).
            if folder.file_name().and_then(|n| n.to_str()) != Some(m.id.as_str()) {
                return None;
            }
            let problem = check_manifest(&m)
                .err()
                .or_else(|| match (m.kind.as_str(), std::fs::read_to_string(folder.join(main_file(&m)))) {
                    ("effect", Ok(code)) => ferrocore::shaderfx::check(&code).err(),
                    (_, Ok(_)) => None,
                    // A module isn't text: there just has to be one.
                    ("logic", Err(_)) if folder.join(&m.module).is_file() => None,
                    (kind, Err(_)) => Some(format!("its {kind} \"{}\" is missing", main_file(&m))),
                })
                .map(|why| format!("It can't run: {why}."));
            Some(PluginInfo {
                on: !off.contains(&m.id),
                id: m.id,
                name: m.name,
                version: m.version,
                author: m.author,
                description: m.description,
                kind: m.kind,
                actions: m.actions,
                problem,
            })
        })
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Hands the effects of the plugins that are on (and can run) to the engine.
pub fn apply(dir: &Path) {
    let effects = list(dir)
        .into_iter()
        .filter(|p| p.on && p.problem.is_none() && p.kind == "effect")
        .filter_map(|p| {
            let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join(&p.id).join("plugin.json")).ok()?).ok()?;
            let code = std::fs::read_to_string(dir.join(&p.id).join(&m.effect)).ok()?;
            Some(ferrocore::shaderfx::PluginEffect { id: p.id, name: p.name, code: code.into() })
        })
        .collect();
    ferrocore::shaderfx::set_plugins(effects);
}

/// Installs (or updates) the plugin in `zip` under `dir` → it, as listed.
pub fn install(dir: &Path, zip: &Path) -> Result<PluginInfo, EngineError> {
    let (m, files) = read_zip(zip)?;
    let io = |e: std::io::Error| EngineError::Internal(e.to_string());
    std::fs::create_dir_all(dir).map_err(io)?;
    // Written beside, then swapped in, so a failure leaves the old version as it was.
    let fresh = dir.join(format!(".{}.new", m.id));
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh).map_err(io)?;
    for (name, bytes) in &files {
        std::fs::write(fresh.join(name), bytes).map_err(io)?;
    }
    let target: PathBuf = dir.join(&m.id);
    let old = dir.join(format!(".{}.old", m.id));
    let _ = std::fs::remove_dir_all(&old);
    if target.exists() {
        std::fs::rename(&target, &old).map_err(io)?;
    }
    if let Err(e) = std::fs::rename(&fresh, &target) {
        let _ = std::fs::rename(&old, &target);
        return Err(io(e));
    }
    let _ = std::fs::remove_dir_all(&old);
    list(dir)
        .into_iter()
        .find(|p| p.id == m.id)
        .ok_or_else(|| EngineError::Internal("the plugin didn't install".into()))
}

/// Turns a plugin on or off → it.
pub fn set_on(dir: &Path, id: &str, on: bool) -> Result<PluginInfo, EngineError> {
    let found = list(dir).into_iter().find(|p| p.id == id).ok_or_else(missing)?;
    let mut off: Vec<String> = off_list(dir).into_iter().filter(|x| x != id).collect();
    if !on {
        off.push(id.to_string());
    }
    write_off(dir, &off)?;
    Ok(PluginInfo { on, ..found })
}

/// Removes a plugin.
pub fn remove(dir: &Path, id: &str) -> Result<(), EngineError> {
    if !good_id(id) || !dir.join(id).is_dir() {
        return Err(missing());
    }
    std::fs::remove_dir_all(dir.join(id)).map_err(|e| EngineError::Internal(e.to_string()))?;
    let off: Vec<String> = off_list(dir).into_iter().filter(|x| x != id).collect();
    write_off(dir, &off)
}

fn missing() -> EngineError {
    EngineError::Missing {
        code: "plugin_not_found",
        message: "There's no such plugin installed.",
        hint: "Refresh the list of plugins.",
    }
}

/// A logic plugin that's on.
#[derive(Debug)]
struct Slot {
    name: String,
    actions: Vec<String>,
    logic: std::sync::Arc<std::sync::Mutex<crate::plugin_logic::Logic>>,
    /// Calls that stopped in a row; three and it's told nothing more until turned on again.
    fails: u32,
    /// Why it stopped being told, shown in the list.
    stopped: Option<String>,
    /// When its last steps ran (at most `PER_MINUTE` a minute).
    recent: std::collections::VecDeque<std::time::Instant>,
}

/// The logic plugins that are on.
#[derive(Debug, Default)]
pub(crate) struct Running {
    slots: std::collections::HashMap<String, Slot>,
}

/// Steps a plugin may run a minute.
pub const PER_MINUTE: usize = 30;

/// An event as plugins hear it: what happened and to whom, never ids, tokens or raw data.
pub fn event_for_plugins(e: &ferrocast_core::event::Event) -> serde_json::Value {
    use ferrocast_core::event::EventKind as K;
    let mut v = serde_json::json!({ "type": e.kind.name(), "platform": e.platform.to_string() });
    if let Some(a) = e.kind.actor() {
        v["name"] = a.display_name.clone().into();
    }
    if let Some(amount) = e.kind.amount_display() {
        v["amount"] = amount.into();
    }
    match &e.kind {
        K::ChatMessage { content, .. } => v["text"] = content.text.clone().into(),
        K::Raid { viewers, .. } => v["viewers"] = (*viewers).into(),
        K::Subscription { months, .. } => v["months"] = (*months).into(),
        K::GiftBatch { count, .. } => v["count"] = (*count).into(),
        K::RewardRedemption { reward, input, .. } => {
            v["reward"] = reward.clone().into();
            if let Some(i) = input {
                v["text"] = i.clone().into();
            }
        }
        K::Tip { message: Some(m), .. } => v["text"] = m.clone().into(),
        _ => {}
    }
    v
}

impl Engine {
    /// Loads the logic plugins that are on (after any change to plugins).
    fn load_logic(&self) {
        let dir = self.plugins_dir();
        let mut slots = std::collections::HashMap::new();
        for p in list(&dir).into_iter().filter(|p| p.on && p.problem.is_none() && p.kind == "logic") {
            let Some(m) = std::fs::read(dir.join(&p.id).join("plugin.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
            else {
                continue;
            };
            match std::fs::read(dir.join(&p.id).join(&m.module))
                .map_err(|e| e.to_string())
                .and_then(|b| crate::plugin_logic::load(&b))
            {
                Ok(logic) => {
                    slots.insert(
                        p.id.clone(),
                        Slot {
                            name: p.name,
                            actions: p.actions,
                            logic: std::sync::Arc::new(std::sync::Mutex::new(logic)),
                            fails: 0,
                            stopped: None,
                            recent: Default::default(),
                        },
                    );
                }
                Err(why) => tracing::warn!(plugin = %p.id, "a logic plugin didn't load: {why}"),
            }
        }
        self.inner.plugin_logic.lock().unwrap_or_else(|e| e.into_inner()).slots = slots;
    }

    /// Tells each logic plugin that's on about `e` (live and test events), off the pipeline.
    pub(crate) fn plugins_saw(&self, e: &ferrocast_core::event::Event) {
        use ferrocast_core::event::Origin;
        if !matches!(e.origin, Origin::Live | Origin::Test) {
            return;
        }
        let todo: Vec<(String, std::sync::Arc<std::sync::Mutex<crate::plugin_logic::Logic>>)> = {
            let r = self.inner.plugin_logic.lock().unwrap_or_else(|e| e.into_inner());
            r.slots.iter().filter(|(_, s)| s.stopped.is_none()).map(|(id, s)| (id.clone(), s.logic.clone())).collect()
        };
        if todo.is_empty() {
            return;
        }
        let event = serde_json::to_vec(&event_for_plugins(e)).unwrap_or_default();
        let who = e.kind.actor().map(|a| a.display_name.clone()).unwrap_or_default();
        for (id, logic) in todo {
            let engine = self.clone();
            let (event, who) = (event.clone(), who.clone());
            tokio::spawn(async move {
                let said =
                    tokio::task::spawn_blocking(move || logic.lock().unwrap_or_else(|e| e.into_inner()).tell(&event))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                engine.plugin_said(&id, said, &who).await;
            });
        }
    }

    /// Runs what a plugin asked for, as far as it's allowed, or counts its failure.
    async fn plugin_said(&self, id: &str, said: Result<crate::plugin_logic::Said, String>, who: &str) {
        let steps = {
            let mut r = self.inner.plugin_logic.lock().unwrap_or_else(|e| e.into_inner());
            let Some(slot) = r.slots.get_mut(id) else { return };
            let said = match said {
                Ok(s) => {
                    slot.fails = 0;
                    s
                }
                Err(why) => {
                    slot.fails += 1;
                    tracing::warn!(plugin = %id, "a logic plugin stopped: {why}");
                    if slot.fails >= 3 {
                        slot.stopped =
                            Some(format!("It stopped three times in a row ({why}); turn it off and on to try again."));
                        let name = slot.name.clone();
                        drop(r);
                        self.plugin_notice(id, format!("{name} was paused: it stopped three times in a row ({why})."));
                    }
                    return;
                }
            };
            for line in &said.logs {
                tracing::info!(plugin = %id, "{}", line.chars().take(300).collect::<String>());
            }
            let now = std::time::Instant::now();
            while slot.recent.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(60)) {
                slot.recent.pop_front();
            }
            let mut steps = Vec::new();
            for v in said.steps {
                let kind = v.get("type").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
                if !slot.actions.contains(&kind) {
                    tracing::warn!(plugin = %id, step = %kind, "a plugin asked for a step it isn't allowed");
                    continue;
                }
                if slot.recent.len() >= PER_MINUTE {
                    tracing::warn!(plugin = %id, "a plugin asked for too many steps this minute");
                    break;
                }
                match serde_json::from_value::<crate::automation::Step>(v) {
                    Ok(step) => {
                        slot.recent.push_back(now);
                        steps.push(step);
                    }
                    Err(e) => tracing::warn!(plugin = %id, "a plugin asked for a step that isn't one: {e}"),
                }
            }
            steps
        };
        for step in steps {
            if let Err(e) = self.automation_step(&step, who).await {
                tracing::warn!(plugin = %id, "a plugin's step failed: {e}");
            }
        }
    }

    fn plugin_notice(&self, id: &str, message: String) {
        self.send_live(crate::frames::LiveFrame::new(
            "notice",
            &crate::frames::Notice {
                key: Some(format!("plugin:{id}")),
                level: crate::frames::NoticeLevel::Warn,
                message,
                action: Some(crate::frames::NoticeAction {
                    label: "Plugins".into(),
                    href: "#/settings/plugins".into(),
                }),
            },
        ));
    }

    /// A panel plugin's page and what it may do → `{ html, actions }`.
    pub async fn plugin_panel(&self, id: &str) -> Result<serde_json::Value, EngineError> {
        let dir = self.plugins_dir();
        let p = self.plugins().await.into_iter().find(|p| p.id == id && p.kind == "panel").ok_or_else(missing)?;
        if !p.on {
            return Err(EngineError::Unavailable(format!("{} is off: turn it on first.", p.name)));
        }
        let m: Manifest = std::fs::read(dir.join(id).join("plugin.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(missing)?;
        let html = std::fs::read_to_string(dir.join(id).join(&m.panel)).map_err(|_| missing())?;
        Ok(serde_json::json!({ "html": html, "actions": p.actions }))
    }

    /// A step a panel asked for, if its plugin is on and allowed it.
    pub async fn plugin_act(&self, id: &str, step: serde_json::Value) -> Result<(), EngineError> {
        let p = self.plugins().await.into_iter().find(|p| p.id == id && p.kind == "panel").ok_or_else(missing)?;
        if !p.on {
            return Err(EngineError::Unavailable(format!("{} is off: turn it on first.", p.name)));
        }
        let kind = step.get("type").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
        if !p.actions.contains(&kind) {
            return Err(refused(
                403,
                "plugin_not_allowed",
                format!("{} isn't allowed to do \"{kind}\".", p.name),
                "Its plugin.json lists what it may do; ask its maker.",
            ));
        }
        let step: crate::automation::Step =
            serde_json::from_value(step).map_err(|e| EngineError::Invalid(format!("That isn't a step: {e}.")))?;
        self.automation_step(&step, "").await
    }

    /// Where plugins are installed.
    pub fn plugins_dir(&self) -> PathBuf {
        self.store().dir().join("plugins")
    }

    /// Loads the installed plugins that are on (at start).
    pub fn load_plugins(&self) {
        apply(&self.plugins_dir());
        self.load_logic();
    }

    pub async fn plugins(&self) -> Vec<PluginInfo> {
        let dir = self.plugins_dir();
        let mut out = tokio::task::spawn_blocking(move || list(&dir)).await.unwrap_or_default();
        // A logic plugin paused for stopping too often says why.
        let r = self.inner.plugin_logic.lock().unwrap_or_else(|e| e.into_inner());
        for p in out.iter_mut().filter(|p| p.problem.is_none()) {
            if let Some(why) = r.slots.get(&p.id).and_then(|s| s.stopped.clone()) {
                p.problem = Some(why);
            }
        }
        out
    }

    /// Installs the plugin file at `zip` (it's checked first) and puts its effect to use.
    pub async fn install_plugin(&self, zip: &Path) -> Result<PluginInfo, EngineError> {
        let (dir, zip) = (self.plugins_dir(), zip.to_path_buf());
        let got = tokio::task::spawn_blocking(move || {
            let r = install(&dir, &zip);
            apply(&dir);
            r
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        self.load_logic();
        self.refresh_studio().await;
        Ok(got)
    }

    pub async fn set_plugin_on(&self, id: &str, on: bool) -> Result<PluginInfo, EngineError> {
        let (dir, id) = (self.plugins_dir(), id.to_string());
        let got = tokio::task::spawn_blocking(move || {
            let r = set_on(&dir, &id, on);
            apply(&dir);
            r
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        self.load_logic();
        self.refresh_studio().await;
        Ok(got)
    }

    pub async fn remove_plugin(&self, id: &str) -> Result<(), EngineError> {
        let (dir, id) = (self.plugins_dir(), id.to_string());
        tokio::task::spawn_blocking(move || {
            let r = remove(&dir, &id);
            apply(&dir);
            r
        })
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))??;
        self.load_logic();
        self.refresh_studio().await;
        Ok(())
    }

    /// The Studio's scenes opened again, so filters using a plugin follow a change at once
    /// (nothing to do when the Studio isn't running).
    async fn refresh_studio(&self) {
        let _ = self.studio_act(crate::studio_host::StudioAct::Refresh).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FX: &str = "fn effect(uv: vec2<f32>) -> vec4<f32> { let c = pic(uv); return vec4<f32>(1.0 - c.rgb, c.a); }";

    fn manifest(id: &str) -> String {
        format!(
            r#"{{ "id": "{id}", "name": "Invert", "version": "1.0.0", "author": "Ana", "description": "Flips colours.", "kind": "effect", "effect": "effect.wgsl" }}"#
        )
    }

    /// A zip of `files` (name, body) at `dir/name.zip`.
    fn zip_of(dir: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(name);
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap_or_else(|e| panic!("{e}")));
        for (n, body) in files {
            w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap_or_else(|e| panic!("{e}"));
            w.write_all(body).unwrap_or_else(|e| panic!("{e}"));
        }
        w.finish().unwrap_or_else(|e| panic!("{e}"));
        path
    }

    fn good(dir: &Path, id: &str) -> PathBuf {
        let m = manifest(id);
        zip_of(dir, &format!("{id}.zip"), &[("plugin.json", m.as_bytes()), ("effect.wgsl", FX.as_bytes())])
    }

    /// The status and code of a refusal.
    fn why(e: EngineError) -> (u16, &'static str) {
        match e {
            EngineError::Refused { status, code, .. } => (status, code),
            EngineError::Missing { code, .. } => (404, code),
            other => panic!("not a refusal: {other}"),
        }
    }

    #[test]
    fn an_effect_plugin_installs_turns_off_and_goes() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = tmp.path().join("plugins");
        let p = install(&dir, &good(tmp.path(), "com.ana.invert")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (p.id.as_str(), p.name.as_str(), p.on, p.problem.as_deref()),
            ("com.ana.invert", "Invert", true, None)
        );
        assert_eq!(list(&dir).len(), 1);
        // In a folder inside the zip works too.
        let m = manifest("com.ana.folder");
        let z =
            zip_of(tmp.path(), "f.zip", &[("Folder/plugin.json", m.as_bytes()), ("Folder/effect.wgsl", FX.as_bytes())]);
        assert!(install(&dir, &z).is_ok());
        // Off, on, gone.
        assert!(!set_on(&dir, "com.ana.invert", false).unwrap_or_else(|e| panic!("{e}")).on);
        assert!(!list(&dir).iter().find(|p| p.id == "com.ana.invert").is_some_and(|p| p.on));
        assert!(set_on(&dir, "com.ana.invert", true).unwrap_or_else(|e| panic!("{e}")).on);
        remove(&dir, "com.ana.invert").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(list(&dir).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["com.ana.folder"]);
        assert_eq!(why(remove(&dir, "com.ana.invert").unwrap_err()), (404, "plugin_not_found"));
        assert_eq!(why(set_on(&dir, "nope.nope", true).unwrap_err()), (404, "plugin_not_found"));
        assert_eq!(why(remove(&dir, "../plugins").unwrap_err()), (404, "plugin_not_found"), "never outside");
    }

    #[test]
    fn only_effects_that_are_on_reach_the_engine() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = tmp.path().join("plugins");
        install(&dir, &good(tmp.path(), "com.ana.invert")).unwrap_or_else(|e| panic!("{e}"));
        apply(&dir);
        assert!(ferrocore::shaderfx::effect_code("plugin:com.ana.invert").is_some_and(|c| c.contains("1.0 - c.rgb")));
        set_on(&dir, "com.ana.invert", false).unwrap_or_else(|e| panic!("{e}"));
        apply(&dir);
        assert!(ferrocore::shaderfx::effect_code("plugin:com.ana.invert").is_none(), "off: gone from the engine");
        // A plugin whose effect broke after it was installed is listed with why, not used.
        set_on(&dir, "com.ana.invert", true).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            dir.join("com.ana.invert/effect.wgsl"),
            "fn effect(uv: vec2<f32>) -> vec4<f32> { return nope; }",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let p = list(&dir).into_iter().next().unwrap_or_else(|| panic!("listed"));
        assert!(p.problem.as_deref().is_some_and(|w| w.contains("nope")), "{:?}", p.problem);
        apply(&dir);
        assert!(ferrocore::shaderfx::effect_code("plugin:com.ana.invert").is_none(), "broken: not used");
        ferrocore::shaderfx::set_plugins(Vec::new());
    }

    #[test]
    fn an_update_replaces_it_and_a_bad_update_keeps_the_old_one() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = tmp.path().join("plugins");
        install(&dir, &good(tmp.path(), "com.ana.invert")).unwrap_or_else(|e| panic!("{e}"));
        let m2 = manifest("com.ana.invert").replace("1.0.0", "2.0.0");
        let z = zip_of(tmp.path(), "v2.zip", &[("plugin.json", m2.as_bytes()), ("effect.wgsl", FX.as_bytes())]);
        assert_eq!(install(&dir, &z).unwrap_or_else(|e| panic!("{e}")).version, "2.0.0");
        let bad = zip_of(
            tmp.path(),
            "v3.zip",
            &[("plugin.json", m2.replace("2.0.0", "3.0.0").as_bytes()), ("effect.wgsl", b"nope")],
        );
        assert!(install(&dir, &bad).is_err());
        assert_eq!(list(&dir)[0].version, "2.0.0", "the working version stays");
    }

    #[test]
    fn a_file_that_isnt_a_good_plugin_says_why_with_its_status() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let d = tmp.path();
        let m = manifest("com.ana.invert");
        let fx = FX.as_bytes();
        let not_zip = d.join("x.zip");
        std::fs::write(&not_zip, b"hello").unwrap_or_else(|e| panic!("{e}"));
        let huge = d.join("huge.zip");
        std::fs::write(&huge, vec![0u8; MAX_ZIP as usize + 1]).unwrap_or_else(|e| panic!("{e}"));
        let big_entry = vec![b' '; MAX_ENTRY as usize + 10];
        let cases: Vec<(PathBuf, (u16, &str))> = vec![
            (not_zip, (415, "plugin_not_a_plugin")),
            (huge, (413, "plugin_too_big")),
            (zip_of(d, "a.zip", &[("effect.wgsl", fx)]), (415, "plugin_not_a_plugin")),
            (zip_of(d, "b.zip", &[("plugin.json", b"{ nope")]), (415, "plugin_not_a_plugin")),
            (zip_of(d, "c.zip", &[("plugin.json", m.as_bytes())]), (415, "plugin_not_a_plugin")),
            (
                zip_of(d, "e.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", b"fn x() {}")]),
                (422, "plugin_effect_broken"),
            ),
            (
                zip_of(d, "f.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", fx), ("evil.dylib", b"MZ")]),
                (415, "plugin_file_type"),
            ),
            (
                zip_of(d, "g.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", fx), ("../../escape.txt", b"x")]),
                (415, "plugin_not_a_plugin"),
            ),
            (
                zip_of(d, "h.zip", &[("plugin.json", m.as_bytes()), ("a/b/effect.wgsl", fx)]),
                (415, "plugin_not_a_plugin"),
            ),
            (
                zip_of(d, "i.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", fx), ("notes.md", &big_entry)]),
                (413, "plugin_too_big"),
            ),
            (
                zip_of(
                    d,
                    "j.zip",
                    &[
                        ("plugin.json", m.replace(r#""kind": "effect""#, r#""kind": "bot""#).as_bytes()),
                        ("effect.wgsl", fx),
                    ],
                ),
                (422, "plugin_manifest"),
            ),
            (
                zip_of(
                    d,
                    "k.zip",
                    &[("plugin.json", m.replace("com.ana.invert", "Bad Id!").as_bytes()), ("effect.wgsl", fx)],
                ),
                (422, "plugin_manifest"),
            ),
            (
                zip_of(
                    d,
                    "l.zip",
                    &[("plugin.json", m.replace("effect.wgsl", "../effect.wgsl").as_bytes()), ("effect.wgsl", fx)],
                ),
                (422, "plugin_manifest"),
            ),
        ];
        for (path, want) in cases {
            let got = why(read_zip(&path).map(|_| ()).unwrap_err());
            assert_eq!(got, want, "{}", path.display());
        }
        // An effect that could run without end is refused; one in a comment or a name isn't.
        let looping = "fn effect(uv: vec2<f32>) -> vec4<f32> { loop { } return pic(uv); }";
        let z = zip_of(d, "n.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", looping.as_bytes())]);
        assert_eq!(why(read_zip(&z).map(|_| ()).unwrap_err()), (422, "plugin_effect_broken"));
        let waiting = "fn effect(uv: vec2<f32>) -> vec4<f32> { var i = 0; while i < 3 { i += 1; } return pic(uv); }";
        let z = zip_of(d, "o.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", waiting.as_bytes())]);
        assert_eq!(why(read_zip(&z).map(|_| ()).unwrap_err()), (422, "plugin_effect_broken"));
        let fine = "// no loop here\nfn effect(uv: vec2<f32>) -> vec4<f32> { let looped = 1.0; var s = vec4<f32>(0.0); for (var i = 0; i < 4; i++) { s += pic(uv); } return s / 4.0 * looped; }";
        let z = zip_of(d, "p.zip", &[("plugin.json", m.as_bytes()), ("effect.wgsl", fine.as_bytes())]);
        assert!(read_zip(&z).is_ok(), "a fixed for loop, a comment and a name are fine");
        // Twenty-one files is too many.
        let many: Vec<(String, Vec<u8>)> = (0..21).map(|i| (format!("n{i}.txt"), b"x".to_vec())).collect();
        let refs: Vec<(&str, &[u8])> = many.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        assert_eq!(why(read_zip(&zip_of(d, "m.zip", &refs)).map(|_| ()).unwrap_err()), (415, "plugin_not_a_plugin"));
        // Nothing was installed by any of them, and nothing was written outside.
        assert!(!d.join("escape.txt").exists());
    }

    /// Leftover work folders and a folder renamed by hand aren't listed (CodeRabbit).
    #[test]
    fn only_folders_named_for_their_plugin_are_listed() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = tmp.path().join("plugins");
        install(&dir, &good(tmp.path(), "com.ana.invert")).unwrap_or_else(|e| panic!("{e}"));
        let copy = |to: &str| {
            std::fs::create_dir_all(dir.join(to)).unwrap_or_else(|e| panic!("{e}"));
            for f in ["plugin.json", "effect.wgsl"] {
                std::fs::copy(dir.join("com.ana.invert").join(f), dir.join(to).join(f))
                    .unwrap_or_else(|e| panic!("{e}"));
            }
        };
        copy(".com.ana.invert.new");
        copy("renamed");
        assert_eq!(list(&dir).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["com.ana.invert"]);
    }

    /// The example plugin in the repo (examples/plugins/scanlines) installs as it is.
    #[test]
    fn the_example_plugin_installs() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/scanlines");
        let read = |f: &str| std::fs::read(src.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        let (m, fx, readme) = (read("plugin.json"), read("effect.wgsl"), read("README.md"));
        let z =
            zip_of(tmp.path(), "scanlines.zip", &[("plugin.json", &m), ("effect.wgsl", &fx), ("README.md", &readme)]);
        let p = install(&tmp.path().join("plugins"), &z).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((p.name.as_str(), p.problem.as_deref()), ("Scanlines", None));
    }

    #[test]
    fn ids_names_and_kinds_are_checked() {
        let m = |id: &str| Manifest {
            id: id.into(),
            name: "X".into(),
            version: "1".into(),
            author: String::new(),
            description: String::new(),
            kind: "effect".into(),
            effect: "e.wgsl".into(),
            module: String::new(),
            panel: String::new(),
            actions: Vec::new(),
        };
        for ok in ["com.ana.invert", "abc", "a-b_c.d1"] {
            assert!(check_manifest(&m(ok)).is_ok(), "{ok}");
        }
        for bad in ["ab", "Com.ana", "1abc", "a..b", "a/b", "a b", &"a".repeat(65)] {
            assert!(check_manifest(&m(bad)).is_err(), "{bad}");
        }
        let mut long = m("abc");
        long.name = "n".repeat(61);
        assert!(check_manifest(&long).is_err());
    }

    /// A logic plugin that switches to "BRB" and starts recording on every event (it's only
    /// allowed scenes), and logs what it heard.
    const LOGIC: &str = r#"
        (module
          (import "ferrocast" "act" (func $act (param i32 i32)))
          (import "ferrocast" "log" (func $log (param i32 i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "{\"type\":\"scene\",\"name\":\"BRB\"}")
          (data (i32.const 64) "{\"type\":\"record\",\"on\":true}")
          (func (export "alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "on_event") (param $p i32) (param $l i32)
            (call $act (i32.const 0) (i32.const 29))
            (call $act (i32.const 64) (i32.const 27))
            (call $log (local.get $p) (local.get $l))))
    "#;

    fn logic_zip(dir: &Path, id: &str, actions: &str, extra: &[(&str, &[u8])]) -> PathBuf {
        let m = format!(
            r#"{{ "id": "{id}", "name": "BRB on follow", "version": "1.0.0", "kind": "logic", "module": "plugin.wasm", "actions": {actions} }}"#
        );
        let wasm = wat::parse_str(LOGIC).unwrap_or_else(|e| panic!("{e}"));
        let mut files: Vec<(&str, &[u8])> = vec![("plugin.json", m.as_bytes()), ("plugin.wasm", &wasm)];
        files.extend_from_slice(extra);
        zip_of(dir, &format!("{id}.zip"), &files)
    }

    #[test]
    fn logic_and_panel_plugins_install_and_bad_ones_say_why() {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let dir = tmp.path().join("plugins");
        let p =
            install(&dir, &logic_zip(tmp.path(), "com.ana.brb", r#"["scene"]"#, &[])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((p.kind.as_str(), p.actions.clone(), p.problem), ("logic", vec!["scene".to_string()], None));
        let html = br#"<button onclick="parent.postMessage({type:'act',step:{type:'save_replay'}},'*')">Save</button>"#;
        let pm = br#"{ "id": "com.ana.panel", "name": "Buttons", "version": "1", "kind": "panel", "panel": "panel.html", "actions": ["save_replay"] }"#;
        let p = install(&dir, &zip_of(tmp.path(), "panel.zip", &[("plugin.json", pm), ("panel.html", html)]))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((p.kind.as_str(), p.problem), ("panel", None));
        // Refusals, each with its code.
        let bad_actions = logic_zip(tmp.path(), "com.ana.bad", r#"["secrets"]"#, &[]);
        assert_eq!(why(install(&dir, &bad_actions).unwrap_err()), (422, "plugin_manifest"));
        let wait = logic_zip(tmp.path(), "com.ana.wait", r#"["wait"]"#, &[]);
        assert_eq!(why(install(&dir, &wait).unwrap_err()), (422, "plugin_manifest"), "waiting isn't an action");
        let page_in_logic = logic_zip(tmp.path(), "com.ana.page", "[]", &[("x.html", b"<p>")]);
        assert_eq!(why(install(&dir, &page_in_logic).unwrap_err()), (415, "plugin_file_type"));
        let fx_with_wasm = zip_of(
            tmp.path(),
            "fxw.zip",
            &[
                ("plugin.json", manifest("com.ana.fxw").as_bytes()),
                ("effect.wgsl", FX.as_bytes()),
                ("x.wasm", b"\0asm"),
            ],
        );
        assert_eq!(why(install(&dir, &fx_with_wasm).unwrap_err()), (415, "plugin_file_type"));
        let m = br#"{ "id": "com.ana.wasi", "name": "W", "version": "1", "kind": "logic", "module": "plugin.wasm" }"#;
        let wasi = wat::parse_str(
            r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)) (memory (export "memory") 1))"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let z = zip_of(tmp.path(), "wasi.zip", &[("plugin.json", m), ("plugin.wasm", &wasi)]);
        assert_eq!(why(install(&dir, &z).unwrap_err()), (422, "plugin_module_broken"));
        let not_wasm = zip_of(tmp.path(), "nw.zip", &[("plugin.json", m), ("plugin.wasm", b"MZ\x90\0")]);
        assert_eq!(why(install(&dir, &not_wasm).unwrap_err()), (422, "plugin_module_broken"));
        let no_module = zip_of(tmp.path(), "nm.zip", &[("plugin.json", m)]);
        assert_eq!(why(install(&dir, &no_module).unwrap_err()), (415, "plugin_not_a_plugin"));
        let exe = zip_of(tmp.path(), "exe.zip", &[("plugin.json", m), ("run.exe", b"MZ")]);
        assert_eq!(why(install(&dir, &exe).unwrap_err()), (415, "plugin_file_type"));
        assert_eq!(list(&dir).len(), 2, "only the good ones");
    }

    /// End to end: a follow reaches the plugin; its allowed step runs, the one it wasn't
    /// allowed doesn't; a panel may do only what it listed.
    #[tokio::test]
    async fn a_logic_plugin_hears_events_and_does_only_what_it_may() {
        use crate::automation::tests::{did, engine, live, until};
        use ferrocast_core::event::{Actor, EventKind, Platform};
        let (e, tmp, studio, _chat) = engine().await;
        let z = logic_zip(tmp.path(), "com.ana.brb", r#"["scene"]"#, &[]);
        e.install_plugin(&z).await.unwrap_or_else(|e| panic!("{e}"));
        e.ingest(live(EventKind::Follow { actor: Actor::named(Platform::Twitch, "Ana") })).await;
        until(|| did(&studio).contains(&"scene BRB".to_string())).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!did(&studio).iter().any(|d| d.starts_with("record")), "record wasn't allowed: {:?}", did(&studio));
        // Off: it hears nothing.
        e.set_plugin_on("com.ana.brb", false).await.unwrap_or_else(|e| panic!("{e}"));
        let before = did(&studio).len();
        e.ingest(live(EventKind::Follow { actor: Actor::named(Platform::Twitch, "Bo") })).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(did(&studio).len(), before);
        // A panel: its page, a step it listed, and one it didn't.
        let html = b"<p>Hi</p>";
        let pm = br#"{ "id": "com.ana.panel", "name": "Buttons", "version": "1", "kind": "panel", "panel": "panel.html", "actions": ["scene"] }"#;
        let z = zip_of(tmp.path(), "panel.zip", &[("plugin.json", pm), ("panel.html", html)]);
        e.install_plugin(&z).await.unwrap_or_else(|e| panic!("{e}"));
        let page = e.plugin_panel("com.ana.panel").await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(page["html"], "<p>Hi</p>");
        e.plugin_act("com.ana.panel", serde_json::json!({ "type": "scene", "name": "Main" }))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(did(&studio).contains(&"scene Main".to_string()));
        let no = e.plugin_act("com.ana.panel", serde_json::json!({ "type": "record", "on": true })).await;
        assert!(matches!(no, Err(EngineError::Refused { status: 403, code: "plugin_not_allowed", .. })));
        let no = e.plugin_act("com.ana.brb", serde_json::json!({ "type": "scene", "name": "Main" })).await;
        assert!(matches!(no, Err(EngineError::Missing { .. })), "a logic plugin has no panel");
        assert!(e.plugin_panel("nope.nope").await.is_err());
    }

    #[test]
    fn plugins_hear_what_happened_never_ids() {
        use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform};
        let mut a = Actor::named(Platform::Twitch, "Ana");
        a.id = Some("secret-user-id".into());
        let mut ev = Event::live(
            Platform::Twitch,
            Some("msg-1".into()),
            Millis(0),
            EventKind::ChatMessage { actor: a, content: Content::plain("!hi"), reply_to: None, is_action: false },
        );
        ev.raw = Some(serde_json::json!({ "token": "x" }));
        let v = event_for_plugins(&ev);
        assert_eq!(v["type"], "chat_message");
        assert_eq!((v["name"].as_str(), v["text"].as_str()), (Some("Ana"), Some("!hi")));
        let s = v.to_string();
        assert!(!s.contains("secret-user-id") && !s.contains("msg-1") && !s.contains("token"), "{s}");
    }
}
