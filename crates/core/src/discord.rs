//! Discord go-live posts through a webhook the streamer pastes in. The webhook URL is a
//! secret (anyone with it can post to the channel): it lives in the secret store (R-45), never in the
//! database, exports or logs. Only a masked hint is ever shown back.

use serde::{Deserialize, Serialize};

use crate::template::{Template, Vars};

/// Discord's message limit is 2,000 characters; leave room for the variables.
pub const MAX_TEMPLATE: usize = 1_500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    /// Post when a platform goes live (needs a webhook too).
    pub enabled: bool,
    pub template: String,
    /// Let the message ping @everyone (off: the text shows but nobody is pinged).
    pub mention_everyone: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        DiscordConfig {
            enabled: true,
            template: "{name} is live on {platform}! {title|default:\"Come hang out.\"} {url}".into(),
            mention_everyone: false,
        }
    }
}

impl DiscordConfig {
    pub fn validate(&self) -> Result<(), String> {
        let t = self.template.trim();
        if t.is_empty() || t.chars().count() > MAX_TEMPLATE {
            return Err("template: write a message of 1 to 1,500 characters.".into());
        }
        Template::parse(t).map_err(|e| format!("template: {e}."))?;
        Ok(())
    }

    pub fn render(&self, vars: &Vars) -> String {
        let text = Template::parse(&self.template).map(|t| t.render(vars)).unwrap_or_default();
        text.chars().take(2_000).collect::<String>().trim().to_string()
    }
}

const WEBHOOK_HELP: &str = "webhook_url: paste the webhook URL from Discord (Server Settings, Integrations, \
Webhooks, Copy Webhook URL). It starts with https://discord.com/api/webhooks/.";

/// Checks a pasted webhook URL: `https://discord.com/api/webhooks/<id>/<token>` (or
/// `discordapp.com`, or with an API version like `/api/v10/`). Nothing else is accepted:
/// no other host, port, login, query or fragment, so a pasted URL can't send our posts
/// anywhere but Discord. Returns it tidied (no trailing slash).
pub fn check_webhook(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.len() > 300 {
        return Err(WEBHOOK_HELP.into());
    }
    let rest = url.strip_prefix("https://").ok_or(WEBHOOK_HELP)?;
    let (host, path) = rest.split_once('/').ok_or(WEBHOOK_HELP)?;
    if !matches!(host.to_ascii_lowercase().as_str(), "discord.com" | "discordapp.com") {
        return Err(WEBHOOK_HELP.into());
    }
    let path = path.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let (id, token) = match parts.as_slice() {
        ["api", "webhooks", id, token] => (*id, *token),
        ["api", v, "webhooks", id, token]
            if v.len() > 1 && v.starts_with('v') && v[1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            (*id, *token)
        }
        _ => return Err(WEBHOOK_HELP.into()),
    };
    let id_ok = (15..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    let token_ok =
        (20..=120).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !id_ok || !token_ok {
        return Err(WEBHOOK_HELP.into());
    }
    Ok(format!("https://{}/{}", host.to_ascii_lowercase(), path))
}

/// What the dashboard shows instead of the secret: "discord.com webhook …4821".
pub fn hint(url: &str) -> String {
    let id = url.trim_end_matches('/').rsplit('/').nth(1).unwrap_or("");
    let tail: String = id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    let host = if url.contains("discordapp.com") { "discordapp.com" } else { "discord.com" };
    format!("{host} webhook …{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "112233445566778899";
    const TOKEN: &str = "AbCdEf_GhIjKl-MnOpQrStUvWxYz0123456789abcdefghij";

    #[test]
    fn accepts_only_real_discord_webhooks() {
        let good = format!("https://discord.com/api/webhooks/{ID}/{TOKEN}");
        assert_eq!(check_webhook(&format!("  {good}/ ")).unwrap(), good);
        assert!(check_webhook(&format!("https://discordapp.com/api/webhooks/{ID}/{TOKEN}")).is_ok());
        assert!(check_webhook(&format!("https://DISCORD.com/api/v10/webhooks/{ID}/{TOKEN}")).is_ok());
        for bad in [
            format!("http://discord.com/api/webhooks/{ID}/{TOKEN}"),
            format!("https://discord.com.evil.example/api/webhooks/{ID}/{TOKEN}"),
            format!("https://evil.example/discord.com/api/webhooks/{ID}/{TOKEN}"),
            format!("https://discord.com:8443/api/webhooks/{ID}/{TOKEN}"),
            format!("https://user@discord.com/api/webhooks/{ID}/{TOKEN}"),
            format!("https://discord.com/api/webhooks/{ID}/{TOKEN}?wait=true"),
            format!("https://discord.com/api/webhooks/{ID}/{TOKEN}#x"),
            format!("https://discord.com/api/webhooks/{ID}/{TOKEN}/github"),
            format!("https://discord.com/api/webhooks/notanid/{TOKEN}"),
            format!("https://discord.com/api/webhooks/{ID}/short"),
            format!("https://discord.com/api/webhooks/{ID}/{TOKEN}{}", "x".repeat(300)),
            "https://discord.com/channels/1/2".into(),
            "discord.com/api/webhooks".into(),
            String::new(),
        ] {
            let e = check_webhook(&bad).unwrap_err();
            assert!(e.starts_with("webhook_url:"), "{bad}");
        }
    }

    #[test]
    fn hints_never_show_the_token() {
        let h = hint(&format!("https://discord.com/api/webhooks/{ID}/{TOKEN}"));
        assert_eq!(h, "discord.com webhook …8899");
        assert!(!h.contains(TOKEN));
    }

    #[test]
    fn templates_render_and_validate() {
        let c = DiscordConfig::default();
        let mut v = Vars::new();
        v.insert("name".into(), "Nova".into());
        v.insert("platform".into(), "Twitch".into());
        v.insert("url".into(), "https://twitch.tv/nova".into());
        assert_eq!(c.render(&v), "Nova is live on Twitch! Come hang out. https://twitch.tv/nova");
        v.insert("title".into(), "Ranked grind".into());
        assert_eq!(c.render(&v), "Nova is live on Twitch! Ranked grind https://twitch.tv/nova");
        assert!(c.validate().is_ok());
        let bad = |t: &str| DiscordConfig { template: t.into(), ..DiscordConfig::default() }.validate().unwrap_err();
        assert!(bad(" ").starts_with("template:"));
        assert!(bad("{oops").starts_with("template:"));
        assert!(bad(&"x".repeat(1_501)).starts_with("template:"));
    }
}
