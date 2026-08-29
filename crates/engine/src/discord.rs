//! Discord go-live posts. The webhook URL is a secret kept in the secret store (R-45), never in
//! the database, so it is never in an export either. Discord's answers become plain
//! messages: a deleted webhook, a bad token, rate limits.

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::discord::{self, DiscordConfig};
use ferrocast_core::event::{Millis, Platform};
use ferrocast_core::template::Vars;
use ferrocast_store::repo;
use serde::Serialize;
use serde_json::{Value, json};

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::{Engine, EngineError, now};

const WEBHOOK_KEY: &str = "discord-webhook";
/// One post per platform per this long: a stream that drops and comes back isn't news.
const REPOST_AFTER_MS: i64 = 30 * 60 * 1000;

/// When each platform last posted (go-live posts are spaced out).
pub(crate) type LastPosts = HashMap<Platform, Millis>;

/// Discord settings as the dashboard sees them: never the URL itself.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordView {
    pub has_webhook: bool,
    /// "discord.com webhook …4821", or null.
    pub webhook_hint: Option<String>,
    pub enabled: bool,
    pub template: String,
    pub mention_everyone: bool,
}

/// A failed post: the error and, for rate limits, how long to wait.
#[derive(Debug)]
pub struct DiscordFailure {
    pub error: EngineError,
    pub retry_after: Option<u64>,
}

impl From<EngineError> for DiscordFailure {
    fn from(error: EngineError) -> Self {
        DiscordFailure { error, retry_after: None }
    }
}

impl Engine {
    fn discord_webhook(&self) -> Result<Option<String>, EngineError> {
        self.inner.secrets.get(WEBHOOK_KEY).map_err(|e| EngineError::Unavailable(e.to_string()))
    }

    async fn discord_config(&self) -> Result<DiscordConfig, EngineError> {
        self.db(|s| repo::settings::get_or_default::<DiscordConfig>(s, "discord")).await
    }

    pub async fn discord(&self) -> Result<DiscordView, EngineError> {
        let cfg = self.discord_config().await?;
        let url = self.discord_webhook()?;
        Ok(DiscordView {
            has_webhook: url.is_some(),
            webhook_hint: url.as_deref().map(discord::hint),
            enabled: cfg.enabled,
            template: cfg.template,
            mention_everyone: cfg.mention_everyone,
        })
    }

    /// Saves what was sent. `webhook_url`: a string replaces it (checked first), `null`
    /// removes it, absent keeps it. Other fields as in `DiscordConfig`; errors name them.
    pub async fn save_discord(&self, patch: &serde_json::Map<String, Value>) -> Result<DiscordView, EngineError> {
        let webhook = match patch.get("webhook_url") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => Some(Some(discord::check_webhook(s).map_err(EngineError::Invalid)?)),
            Some(_) => return Err(EngineError::Invalid("webhook_url: must be text, or null to remove it.".into())),
        };
        {
            let _one_at_a_time = self.config_lock().await;
            let mut rest = patch.clone();
            rest.remove("webhook_url");
            let mut cfg = crate::patch::merge(&self.discord_config().await?, &rest)?;
            cfg.template = cfg.template.trim().to_string();
            cfg.validate().map_err(EngineError::Invalid)?;
            self.db(move |s| repo::settings::set(s, "discord", &cfg)).await?;
            let r = match webhook {
                None => Ok(()),
                Some(None) => self.inner.secrets.delete(WEBHOOK_KEY),
                Some(Some(url)) => self.inner.secrets.set(WEBHOOK_KEY, &url),
            };
            r.map_err(|e| EngineError::Unavailable(format!("The webhook couldn't be saved: {e}")))?;
        }
        self.discord().await
    }

    /// Posts a test message. 422 without a webhook; Discord's refusals as plain messages.
    pub async fn test_discord(&self) -> Result<(), DiscordFailure> {
        let url = self
            .discord_webhook()?
            .ok_or_else(|| EngineError::Unavailable("Paste a Discord webhook URL first, then send a test.".into()))?;
        self.post_discord(&url, "Test from Ferrocast: your go-live posts will show up here.", false).await
    }

    /// A platform went live: post the template once (not again for 30 minutes on that
    /// platform). In the background; a failure is one dashboard notice.
    pub(crate) fn discord_go_live(&self, platform: Platform, title: Option<String>) {
        let engine = self.clone();
        tokio::spawn(async move {
            let Ok(Some(url)) = engine.discord_webhook() else { return };
            let Ok(cfg) = engine.discord_config().await else { return };
            if !cfg.enabled {
                return;
            }
            let at = now();
            {
                let mut last = engine.inner.discord_posts.lock().unwrap_or_else(|p| p.into_inner());
                if last.get(&platform).is_some_and(|t| at.0 - t.0 < REPOST_AFTER_MS) {
                    return;
                }
                last.insert(platform, at);
            }
            let vars = engine.go_live_vars(platform, title).await;
            let text = cfg.render(&vars);
            if text.is_empty() {
                return;
            }
            if let Err(f) = engine.post_discord(&url, &text, cfg.mention_everyone).await {
                engine.send_live(LiveFrame::new(
                    "notice",
                    &Notice {
                        key: Some("discord".into()),
                        level: NoticeLevel::Warn,
                        message: format!("The Discord go-live post didn't go out. {}", f.error),
                        action: Some(crate::frames::NoticeAction {
                            label: "Discord settings".into(),
                            href: "#/settings/discord".into(),
                        }),
                    },
                ));
            }
        });
    }

    async fn go_live_vars(&self, platform: Platform, title: Option<String>) -> Vars {
        let key = crate::platform_key(platform).to_string();
        let account =
            self.db(repo::settings::accounts).await.ok().and_then(|list| list.into_iter().find(|a| a.platform == key));
        let mut v = Vars::new();
        v.insert("platform".into(), platform.label().into());
        v.insert("title".into(), title.unwrap_or_default());
        if let Some(a) = account {
            v.insert("name".into(), a.display_name.clone());
            let url = match platform {
                Platform::Twitch => format!("https://twitch.tv/{}", a.login),
                Platform::Kick => format!("https://kick.com/{}", a.login),
                Platform::Youtube => format!("https://www.youtube.com/channel/{}/live", a.user_id),
                _ => String::new(),
            };
            v.insert("url".into(), url);
        } else {
            v.insert("name".into(), "The stream".into());
        }
        v
    }

    async fn post_discord(&self, url: &str, text: &str, everyone: bool) -> Result<(), DiscordFailure> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        let body = json!({
            "content": text,
            "allowed_mentions": { "parse": if everyone { vec!["everyone"] } else { vec![] } },
        });
        let res = http.post(url).json(&body).send().await.map_err(|e| {
            // Never put the URL (it holds the token) in the message or the log.
            tracing::warn!(error = %e.without_url(), "couldn't reach Discord");
            EngineError::Unavailable("Couldn't reach Discord. Check your internet connection and try again.".into())
        })?;
        let status = res.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let header_wait =
            res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        let detail: Value = res.json().await.unwrap_or(Value::Null);
        let message = detail.get("message").and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>();
        Err(match status {
            404 => EngineError::Unavailable(
                "Discord says this webhook was deleted. Make a new one in Discord (Server Settings, Integrations, \
                 Webhooks) and paste it here."
                    .into(),
            )
            .into(),
            401 | 403 => EngineError::Unavailable(
                "Discord refused this webhook. Copy the webhook URL again from Discord and paste it here.".into(),
            )
            .into(),
            429 => {
                let wait = detail.get("retry_after").and_then(Value::as_f64).or(header_wait).unwrap_or(5.0);
                let secs = (wait.ceil() as u64).clamp(1, 3_600);
                DiscordFailure {
                    error: EngineError::TooMany(format!(
                        "Discord is limiting posts to this webhook. Try again in {secs} seconds."
                    )),
                    retry_after: Some(secs),
                }
            }
            400 => EngineError::Invalid(format!(
                "template: Discord didn't accept the message{}.",
                if message.is_empty() { String::new() } else { format!(" ({message})") }
            ))
            .into(),
            _ => EngineError::Unavailable(format!("Discord had a problem (HTTP {status}). Try again in a minute."))
                .into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_discord_webhook_for_tests(&self, url: &str) {
        let _ = self.inner.secrets.set(WEBHOOK_KEY, url);
    }
}
