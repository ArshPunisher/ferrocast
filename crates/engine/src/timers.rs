//! Bot timers: post scheduled messages while live, only after enough chat (R-11).

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::event::{Millis, Platform};
use ferrocast_core::template::{Template, Vars};

use crate::{Engine, now};

const TICK: Duration = Duration::from_secs(20);

/// When a timer last posted on a platform, and how many chat lines that platform had then.
type LastPost = HashMap<(String, Platform), (Millis, u32)>;

pub(crate) async fn run(engine: Engine) {
    let mut last: LastPost = HashMap::new();
    loop {
        tokio::time::sleep(TICK).await;
        tick(&engine, &mut last, now());
    }
}

/// One pass over the timers; returns what was posted (for tests).
pub(crate) fn tick(engine: &Engine, last: &mut LastPost, t: Millis) -> Vec<(String, Platform)> {
    let (timers, live, lines, bot_on) = {
        let st = engine.state();
        (st.timers.clone(), st.live.clone(), st.session.chat_lines.clone(), st.settings.bot_enabled)
    };
    let mut posted = Vec::new();
    if !bot_on {
        return posted;
    }
    for timer in timers {
        let targets = if timer.platforms.is_empty() { Platform::CHAT.to_vec() } else { timer.platforms.clone() };
        for p in targets {
            let is_live = live.get(&p).copied().unwrap_or(false);
            let now_lines = lines.get(&p).copied().unwrap_or(0);
            let key = (timer.id.clone(), p);
            let prev = last.get(&key).copied();
            let since = now_lines.saturating_sub(prev.map(|(_, l)| l).unwrap_or(0));
            if !timer.due(prev.map(|(at, _)| at), since, is_live, t) {
                continue;
            }
            let mut vars = Vars::new();
            vars.insert("platform".into(), p.label().into());
            let text = Template::parse(&timer.message).map(|tpl| tpl.render(&vars)).unwrap_or_default();
            let msg = ferrocast_core::command::safe_reply(&text);
            if msg.is_empty() {
                continue;
            }
            engine.spawn_action(p, "post a timer message", move |c| {
                Box::pin(async move { c.send_chat(&msg, None).await })
            });
            last.insert(key, (t, now_lines));
            posted.push((timer.id.clone(), p));
        }
    }
    posted
}
