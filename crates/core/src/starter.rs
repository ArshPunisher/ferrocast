//! The starter command pack: four commands most streams want, added in one click.

use crate::command::{Access, Command};

/// `!socials`, `!lurk`, `!uptime` and `!so`. Ids are blank (the engine picks them).
/// `{uptime_text}` and `{target_game_note}` are filled in by the engine when the command runs.
pub fn starter_commands() -> Vec<Command> {
    let cmd = |name: &str, response: &str, access: Access, cooldown_s: u32, user_cooldown_s: u32| Command {
        id: String::new(),
        name: name.into(),
        aliases: vec![],
        response: response.into(),
        access,
        cooldown_s,
        user_cooldown_s,
        platforms: vec![],
        enabled: true,
    };
    vec![
        cmd(
            "!socials",
            "Find me everywhere: add your links to this reply in Ferrocast (Bot, !socials).",
            Access::Everyone,
            30,
            0,
        ),
        cmd("!lurk", "{user} is lurking. Thanks for hanging out!", Access::Everyone, 0, 300),
        cmd("!uptime", "{uptime_text}", Access::Everyone, 10, 0),
        cmd("!so", "Go show {target} some love and give them a follow! {target_game_note}", Access::Mods, 5, 0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pack_is_valid_and_has_unique_names() {
        let all = starter_commands();
        for (i, c) in all.iter().enumerate() {
            let others: Vec<Command> = all
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(j, o)| Command { id: j.to_string(), ..o.clone() })
                .collect();
            let me = Command { id: i.to_string(), ..c.clone() };
            assert!(me.validate(&others).is_ok(), "{}", c.name);
        }
        assert_eq!(all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["!socials", "!lurk", "!uptime", "!so"]);
        assert_eq!(all[3].access, Access::Mods, "shoutouts are for mods");
    }
}
