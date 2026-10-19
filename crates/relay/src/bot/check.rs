//! `ferrocast-relay bot-check [<install-id>]`: the owner's look at the Twitch cloud bot from
//! the server (run by `deploy/bot-check.sh`). It builds the same Twitch client the server
//! does from the env, gets an app token and prints, without any token or secret: the conduit
//! (the id kept in `bot_kv` and the ones Twitch has), shard 0's status and callback, each
//! bound channel's row (accounts, ready or blocked, last error, the switch, Pro, the pushed
//! config's version and commands) and the EventSub subscriptions Twitch has for it.
//!
//! It runs in a process of its own: who's on duty (the 90 s lease, PCs' `bot_seen`) and the
//! running relay's memory can't be seen from here, and the output says so.

use std::fmt::Write as _;

use ferrocast_core::cloudbot::BotConfig;
use serde_json::Value;

use super::store::{self, Channel};
use super::twitch::{ShardInfo, SubInfo, TwitchCloud};
use super::{TAKEOVER_AFTER, sub_is_on, sub_is_pro};
use crate::db::Db;

/// The EventSub types the cloud bot subscribes per channel.
pub const KINDS: [&str; 3] = ["channel.chat.message", "stream.online", "stream.offline"];

fn when(ms: Option<i64>) -> String {
    match ms {
        Some(ms) => {
            let ago = (crate::now_ms() - ms) / 1000;
            format!("{ms} ({}m {}s ago)", ago / 60, ago % 60)
        }
        None => "never".into(),
    }
}

/// The install's subscription id, or every subscription with a Twitch channel.
fn channels(db: &Db, install: Option<&str>) -> Result<Vec<Channel>, String> {
    let all = db.bot_all_channels().map_err(|e| format!("Couldn't read the cloud bot channels: {e}"))?;
    let Some(install) = install else { return Ok(all) };
    let sub = db
        .pro_sub_for_install(install)
        .map_err(|e| format!("Couldn't read the install's Pro: {e}"))?
        .ok_or_else(|| format!("No Pro subscription is linked to install '{install}' on this relay."))?;
    Ok(all.into_iter().filter(|c| c.sub_id == sub.id).collect())
}

fn config_line(db: &Db, sub: &str) -> String {
    match db.bot_config(sub) {
        Ok(Some(row)) => {
            let cfg: Option<BotConfig> = serde_json::from_str(&row.json).ok();
            let mut s = format!(
                "switch {}, config version {} (pushed {})",
                if row.enabled { "ON" } else { "off" },
                row.version,
                when((row.version > 0).then_some(row.updated_at))
            );
            match cfg {
                Some(c) => {
                    let cmds: Vec<String> = c
                        .commands
                        .iter()
                        .map(|c| if c.enabled { c.name.clone() } else { format!("{} (off)", c.name) })
                        .collect();
                    let _ = write!(
                        s,
                        "\n    desktop bot switch (bot_on): {}\n    commands: {}\n    timers {}, auto-replies {}, counters {}",
                        if c.bot_on { "on" } else { "OFF: the cloud stays quiet too" },
                        if cmds.is_empty() { "none".into() } else { cmds.join(", ") },
                        c.timers.len(),
                        c.auto_replies.len(),
                        c.counters.len()
                    );
                }
                None if row.version == 0 => {
                    s.push_str("\n    no config pushed yet: the cloud has nothing to answer with")
                }
                None => s.push_str("\n    the stored config doesn't parse"),
            }
            s
        }
        Ok(None) => "no bot_config row: never switched on, no config pushed".into(),
        Err(e) => format!("couldn't read the config: {e}"),
    }
}

fn channel_block(db: &Db, ch: &Channel) -> String {
    let acct = |id: &Option<String>, login: &Option<String>| match id {
        Some(i) => format!("{} ({i})", login.as_deref().unwrap_or("?")),
        None => "none".into(),
    };
    let state = if ch.blocked() {
        "BLOCKED"
    } else if ch.ready() {
        "ready"
    } else {
        "needs the channel step"
    };
    let mut s = String::new();
    let _ = writeln!(s, "- {} channel of subscription {}", ch.platform, ch.sub_id);
    let _ = writeln!(s, "    bot account: {}", acct(&ch.bot_id, &ch.bot_login));
    let _ = writeln!(s, "    channel: {}", acct(&ch.channel_id, &ch.channel_login));
    let _ = writeln!(s, "    state: {state}; moderation {}", if ch.moderation { "on" } else { "off" });
    let _ = writeln!(
        s,
        "    last error: {}",
        ch.error.as_ref().map_or_else(
            || "none".into(),
            |e| format!("{} ({})", e["code"].as_str().unwrap_or("?"), e["message"].as_str().unwrap_or(""))
        )
    );
    let _ = writeln!(
        s,
        "    live: {}; last reply {}; last drop {}",
        ch.live_since.map_or_else(|| "offline".into(), |t| format!("since {t}")),
        when(ch.last_reply_at),
        ch.last_drop.as_ref().map_or_else(|| "none".into(), Value::to_string)
    );
    let _ = writeln!(
        s,
        "    Pro active: {}; cloud bot on (switch and Pro): {}",
        if sub_is_pro(db, &ch.sub_id) { "yes" } else { "NO" },
        match sub_is_on(db, &ch.sub_id) {
            Ok(true) => "yes",
            Ok(false) => "NO",
            Err(_) => "unknown",
        }
    );
    let _ = writeln!(s, "    {}", config_line(db, &ch.sub_id));
    let _ = writeln!(
        s,
        "    EventSub ids saved here: {}",
        if ch.subscriptions.is_empty() { "NONE".into() } else { ch.subscriptions.join(", ") }
    );
    s
}

fn subs_block(ch: &Channel, conduit: Option<&str>, subs: &[SubInfo]) -> String {
    let mut s = String::new();
    let Some(channel) = ch.channel_id.as_deref() else { return s };
    let mine: Vec<&SubInfo> =
        subs.iter().filter(|x| x.condition["broadcaster_user_id"].as_str() == Some(channel)).collect();
    let _ = writeln!(s, "    EventSub at Twitch for this channel: {}", mine.len());
    for x in &mine {
        let _ = writeln!(
            s,
            "      {} {} status={} condition={} conduit={}{}{}",
            x.kind,
            x.id,
            x.status,
            x.condition,
            x.conduit.as_deref().unwrap_or("(not a conduit)"),
            if conduit.is_some() && x.conduit.as_deref() != conduit { "  <- NOT the current conduit" } else { "" },
            if ch.subscriptions.contains(&x.id) { "" } else { "  (id not saved here)" },
        );
    }
    for kind in KINDS {
        if !mine.iter().any(|x| x.kind == kind && x.status == "enabled") {
            let _ = writeln!(s, "      MISSING: no enabled {kind}");
        }
    }
    if let Some(chat) = mine.iter().find(|x| x.kind == "channel.chat.message")
        && chat.condition["user_id"].as_str() != ch.bot_id.as_deref()
    {
        let _ = writeln!(s, "      the chat subscription's user_id isn't the bot account saved here");
    }
    s
}

fn shard_line(shards: &[ShardInfo], want: &str) -> String {
    match shards.iter().find(|s| s.id == "0") {
        Some(s) => {
            let cb = s.callback.as_deref().unwrap_or("none");
            let mut out = format!("shard 0: status={} method={} callback={cb}", s.status, s.method);
            if s.status != "enabled" {
                out.push_str("\n  PROBLEM: shard 0 isn't enabled, so Twitch delivers no chat to the relay");
            }
            if cb != want {
                out.push_str(&format!("\n  PROBLEM: the callback isn't this relay's ({want})"));
            }
            out
        }
        None => "shard 0: MISSING".into(),
    }
}

/// The whole report. `api` is `None` when the Twitch cloud app isn't configured.
pub async fn report(
    db: &Db,
    api: Option<&dyn TwitchCloud>,
    public_url: &str,
    install: Option<&str>,
) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Cloud bot check. This runs in its own process: who's on duty right now (the {} s lease that PCs' \
         bot_seen keep), the running relay's memory (cached config, conduit, cooldowns) and its logs can't be \
         seen from here. The Bot page shows duty; `docker logs` shows what the relay did.",
        TAKEOVER_AFTER.as_secs()
    );
    let want = format!("{public_url}/v1/bot/twitch/eventsub");
    let _ = writeln!(out, "Callback this relay expects: {want}");
    let chans = channels(db, install)?;
    let kv = db.bot_kv(store::KV_CONDUIT).ok().flatten();
    let _ = writeln!(out, "\nConduit saved here (bot_kv): {}", kv.as_deref().unwrap_or("none"));
    let mut conduit_now: Option<String> = None;
    let mut all_subs: Vec<SubInfo> = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    match api {
        None => {
            let _ = writeln!(out, "Twitch cloud app: NOT configured (TWITCH_CLOUD_* unset): nothing asked of Twitch");
        }
        Some(api) => match api.app_token().await {
            Err(e) => {
                let _ = writeln!(out, "Twitch app token: FAILED ({e}): check TWITCH_CLOUD_CLIENT_ID / _SECRET");
            }
            Ok((app, _)) => {
                let _ = writeln!(out, "Twitch app token: ok");
                match api.conduits(&app).await {
                    Ok(ids) => {
                        let _ = writeln!(
                            out,
                            "Conduits at Twitch: {}",
                            if ids.is_empty() { "NONE".into() } else { ids.join(", ") }
                        );
                        if kv.as_ref().is_some_and(|k| !ids.contains(k)) {
                            let _ = writeln!(out, "  PROBLEM: the saved conduit isn't one Twitch has");
                        }
                        conduit_now = kv.clone().filter(|k| ids.contains(k)).or_else(|| ids.first().cloned());
                    }
                    Err(e) => {
                        let _ = writeln!(out, "Conduits at Twitch: couldn't list ({e})");
                    }
                }
                if let Some(c) = &conduit_now {
                    match api.shards(&app, c).await {
                        Ok(sh) => {
                            let _ = writeln!(out, "Conduit {c} {}", shard_line(&sh, &want));
                        }
                        Err(e) => {
                            let _ = writeln!(out, "Shards of {c}: couldn't list ({e})");
                        }
                    }
                }
                let mut asked: Vec<&str> = Vec::new();
                for ch in chans.iter().filter(|c| c.platform == "twitch") {
                    if let Some(id) = ch.channel_id.as_deref().filter(|id| !asked.contains(id)) {
                        asked.push(id);
                        match api.subscriptions(&app, id).await {
                            Ok(s) => {
                                all_subs.extend(s);
                                listed.push(id.to_string());
                            }
                            Err(e) => {
                                let _ = writeln!(out, "EventSub for channel {id}: couldn't list ({e})");
                            }
                        }
                    }
                }
            }
        },
    }
    let _ = writeln!(out, "\nChannels ({}):", chans.len());
    if chans.is_empty() {
        let _ = writeln!(out, "  none bound{}", install.map(|i| format!(" for install {i}")).unwrap_or_default());
    }
    for ch in &chans {
        out.push_str(&channel_block(db, ch));
        if ch.platform == "twitch" && api.is_some() {
            if ch.channel_id.as_ref().is_some_and(|id| listed.contains(id)) {
                out.push_str(&subs_block(ch, conduit_now.as_deref(), &all_subs));
            } else if ch.channel_id.is_some() {
                out.push_str("    EventSub at Twitch for this channel: not checked (see the error above)\n");
            }
        }
    }
    Ok(out.trim_end().to_string())
}

/// The command line: `bot-check [<install-id>]`.
pub fn parse_args(args: &[String]) -> Result<Option<&str>, String> {
    match args {
        [] => Ok(None),
        [id] if !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok(Some(id.as_str()))
        }
        [_] => Err("That doesn't look like an install id (only the part before the dot).".into()),
        _ => Err("Use: bot-check [<install-id>]".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::billing::store::Synced;
    use crate::bot::tests::{BOT_ID, FakeTwitch, STREAMER_ID};
    use serde_json::json;

    fn db_with_channel(dir: &std::path::Path) -> Db {
        let db = Db::open(&dir.join("r.db")).unwrap();
        db.add_install("inst-0001", &crate::hash("secret")).unwrap();
        db.add_pro_sub("sub_1", "monthly", "e", "created", "inst-0001").unwrap();
        db.sync_pro_sub(
            None,
            "sub_1",
            &Synced { status: "active".into(), current_end: Some(crate::now_ms() + 86_400_000), cancel_at_end: None },
        )
        .unwrap();
        db.bot_set_account("sub_1", "twitch", (BOT_ID, "ashabot"), None, "", false, "inst-0001", 1).unwrap();
        db.bot_set_channel("sub_1", "twitch", STREAMER_ID, "asha_live").unwrap();
        db
    }

    #[test]
    fn arguments_are_an_optional_install_id() {
        assert_eq!(parse_args(&[]).unwrap(), None);
        assert_eq!(parse_args(&["abc-123_X".into()]).unwrap(), Some("abc-123_X"));
        assert!(parse_args(&["abc.secret".into()]).is_err());
        assert!(parse_args(&["a".into(), "b".into()]).is_err());
    }

    #[tokio::test]
    async fn without_twitch_it_still_reads_the_database_and_says_what_it_cant_see() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_with_channel(dir.path());
        let out = report(&db, None, "https://relay.test", None).await.unwrap();
        assert!(out.contains("its own process") && out.contains("lease"), "{out}");
        assert!(out.contains("NOT configured"), "{out}");
        assert!(out.contains("asha_live (141981764)") && out.contains("ashabot (500000001)"), "{out}");
        assert!(out.contains("switched on") || out.contains("never switched on"), "{out}");
        assert!(out.contains("EventSub ids saved here: NONE"), "{out}");
        let err = report(&db, None, "https://relay.test", Some("nope-0001")).await.unwrap_err();
        assert!(err.contains("No Pro subscription"), "{err}");
    }

    #[tokio::test]
    async fn it_names_a_failed_shard_and_missing_subscriptions_without_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_with_channel(dir.path());
        let cfg = json!({ "bot_on": true, "commands": [] });
        db.bot_push_config("sub_1", 0, &cfg.to_string(), 0, 1).unwrap();
        db.bot_set_enabled("sub_1", true, 1).unwrap();
        db.bot_put_kv(store::KV_CONDUIT, "conduit-1").unwrap();
        let tw = Arc::new(FakeTwitch::default());
        tw.with(|t| {
            t.subscribed.push((
                "stream.online".into(),
                json!({ "broadcaster_user_id": STREAMER_ID }),
                "conduit-1".into(),
            ))
        });
        tw.conduit("app-token", "https://elsewhere.test/v1/bot/twitch/eventsub", crate::bot::tests::ES_SECRET)
            .await
            .unwrap();
        tw.with(|t| t.shard_status = Some("webhook_callback_verification_failed".into()));
        let out = report(&db, Some(tw.as_ref()), "https://relay.test", Some("inst-0001")).await.unwrap();
        assert!(out.contains("Conduits at Twitch: conduit-1"), "{out}");
        assert!(out.contains("status=webhook_callback_verification_failed"), "{out}");
        assert!(out.contains("isn't enabled") && out.contains("callback isn't this relay's"), "{out}");
        assert!(out.contains("MISSING: no enabled channel.chat.message"), "{out}");
        assert!(!out.contains("MISSING: no enabled stream.online"), "{out}");
        assert!(out.contains("switch ON, config version 1"), "{out}");
        assert!(!out.contains("app-token") && !out.contains("SECRET"), "no tokens or secrets: {out}");
    }
}
