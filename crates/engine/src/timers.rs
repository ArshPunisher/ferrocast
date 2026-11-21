//! Bot timers: post scheduled messages while live, only after enough chat (R-11).

use std::collections::HashMap;
use std::time::Duration;

use ferrocast_core::event::{Millis, Platform};
use ferrocast_core::template::Vars;

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
    // Drop entries for deleted timers so `last` never grows across create/delete cycles.
    let active: std::collections::HashSet<&str> = timers.iter().map(|tm| tm.id.as_str()).collect();
    last.retain(|(id, _), _| active.contains(id.as_str()));
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
            engine.counter_vars(&mut vars); // `{count:deaths}` (Studio counters)
            // The same text the cloud bot posts (ferrocast_core::cloudbot).
            let msg = ferrocast_core::cloudbot::timer_text(&timer, &vars, p);
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn deleted_timers_are_pruned_from_last_post() {
        let dir = tempfile::tempdir().unwrap();
        let secrets: Arc<dyn ferrocast_secrets::Secrets> = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let engine = Engine::start_with_secrets(dir.path(), secrets).await.unwrap();
        let mut last: LastPost = HashMap::new();
        for i in 0..1_000 {
            last.insert((format!("deleted-{i}"), Platform::Twitch), (Millis(i * 1000), 10));
        }
        let _ = tick(&engine, &mut last, Millis(2_000_000));
        assert!(last.is_empty(), "deleted timer entries must be pruned, left {}", last.len());
    }
}
