//! The starter command pack and the shoutout's last-known game.

use std::time::Duration;

use ferrocast_core::command::{self, Command};
use ferrocast_core::event::{Actor, Platform};
use ferrocast_core::template::Vars;
use serde::Serialize;

use crate::{Engine, EngineError};

/// How long a shoutout waits for the platform to say what someone last played.
const GAME_LOOKUP: Duration = Duration::from_secs(2);

/// What "Add starter commands" did.
#[derive(Debug, Clone, Serialize)]
pub struct StarterResult {
    pub added: Vec<Command>,
    /// Names that were already taken (by a command or an alias), left alone.
    pub skipped: Vec<String>,
}

impl Engine {
    /// Adds `!socials`, `!lurk`, `!uptime` and `!so`, skipping any name already in use.
    pub async fn add_starter_commands(&self) -> Result<StarterResult, EngineError> {
        let mut out = StarterResult { added: vec![], skipped: vec![] };
        for mut c in ferrocast_core::starter::starter_commands() {
            let taken = self.commands().iter().any(|o| o.answers_to(&c.name));
            if taken {
                out.skipped.push(c.name);
                continue;
            }
            c.id = ferrocast_store::new_id();
            let name = c.name.clone();
            match self.save_command(c).await {
                Ok(saved) => out.added.push(saved),
                // Another click got there first: the name is in use now, which is the goal.
                Err(EngineError::Store(ferrocast_store::StoreError::Conflict(_))) => out.skipped.push(name),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// For a command whose reply mentions the target's game (`!so @friend`): asks the
    /// platform what they last played. `{target_game}` is the game or empty;
    /// `{target_game_note}` is "They were last playing X." or empty.
    pub(crate) async fn shoutout_vars(&self, platform: Platform, actor: &Actor, text: &str, vars: &mut Vars) {
        vars.insert("target_game".into(), String::new());
        vars.insert("target_game_note".into(), String::new());
        let Some(inv) = command::parse(text) else { return };
        // Only for someone allowed to run the command, and not during its cooldown: a
        // viewer spamming !so must not spend the platform's request budget.
        let wants = {
            let st = self.state();
            let cd = self.inner.cooldowns.lock().unwrap_or_else(|p| p.into_inner());
            st.commands.iter().any(|c| {
                c.answers_to(inv.word) && c.response.contains("target_game") && cd.ready(c, actor, crate::now())
            })
        };
        let Some(login) = inv.args.first().map(|a| a.trim_start_matches('@').to_string()) else { return };
        if !wants || login.is_empty() {
            return;
        }
        let Some(c) = self.connector(platform) else { return };
        let game = match tokio::time::timeout(GAME_LOOKUP, c.last_game(&login)).await {
            Ok(Ok(Some(g))) => g,
            Ok(Err(err)) => {
                tracing::debug!(%err, "couldn't look up a shoutout's game");
                return;
            }
            _ => return,
        };
        vars.insert("target_game_note".into(), format!("They were last playing {game}."));
        vars.insert("target_game".into(), game);
    }
}
