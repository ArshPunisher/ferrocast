//! Giveaways: viewers type a keyword (`!join`) to enter, optionally paying loyalty points;
//! subscribers can get a second ticket; the streamer and bots can't enter; the winner is
//! drawn with randomness the engine takes from the OS, without modulo bias.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::event::{Actor, Millis, Platform, Roles};
use crate::loyalty::{MAX_ADJUST, is_audience};

/// More entrants than any real chat; past this, entries are refused instead of growing memory.
pub const MAX_ENTRANTS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiveawaySpec {
    pub prize: String,
    pub keyword: String,
    /// Loyalty points to enter (0 = free).
    pub cost: i64,
    /// Subscribers and members get two tickets.
    pub subscriber_bonus: bool,
}

impl GiveawaySpec {
    /// Trims and checks everything; errors name the field.
    pub fn cleaned(&self) -> Result<GiveawaySpec, String> {
        let prize = self.prize.split_whitespace().collect::<Vec<_>>().join(" ");
        if prize.is_empty() || prize.chars().count() > 100 {
            return Err("prize: say what they can win, in 1 to 100 characters.".into());
        }
        let keyword = self.keyword.trim().to_lowercase();
        let rest = keyword.strip_prefix('!').unwrap_or("");
        if rest.is_empty() || rest.len() > 32 || !rest.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            return Err("keyword: one word starting with !, like !join.".into());
        }
        if !(0..=MAX_ADJUST).contains(&self.cost) {
            return Err("cost: must be a whole number from 0 to 1,000,000,000 points.".into());
        }
        Ok(GiveawaySpec { prize, keyword, cost: self.cost, subscriber_bonus: self.subscriber_bonus })
    }
}

/// Someone who entered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entrant {
    pub platform: Platform,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub tickets: u8,
    /// Points paid to enter (refunded if the giveaway is cancelled).
    pub paid: i64,
    /// The person (linked accounts share one), so nobody enters twice from two platforms.
    #[serde(skip)]
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Giveaway {
    pub id: String,
    pub prize: String,
    pub keyword: String,
    pub cost: i64,
    pub subscriber_bonus: bool,
    pub started_at: Millis,
    /// Taking entries.
    pub open: bool,
    /// Finished or cancelled: nothing changes any more.
    pub ended: bool,
    pub entrants: Vec<Entrant>,
    pub winners: Vec<Entrant>,
    keys: HashSet<String>,
}

/// Why an entry didn't count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotEntered {
    Closed,
    Already,
    /// The streamer, a bot, or someone without an id.
    NotAllowed,
    Full,
}

/// The dashboard's view: every entrant (newest first, up to 500) and the winners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiveawayView {
    pub id: String,
    pub prize: String,
    pub keyword: String,
    pub cost: i64,
    pub subscriber_bonus: bool,
    pub started_at: Millis,
    pub open: bool,
    pub ended: bool,
    pub entries: u64,
    pub tickets: u64,
    pub entrants: Vec<EntrantView>,
    pub winners: Vec<EntrantView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrantView {
    pub platform: Platform,
    pub user_id: String,
    pub login: String,
    pub display_name: String,
    pub tickets: u8,
}

/// What an overlay needs: small enough to send on every entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GiveawayCard {
    pub id: String,
    pub prize: String,
    pub keyword: String,
    pub open: bool,
    pub entries: u64,
    /// The latest winner.
    pub winner: Option<EntrantView>,
}

fn entrant_view(e: &Entrant) -> EntrantView {
    EntrantView {
        platform: e.platform,
        user_id: e.user_id.clone(),
        login: e.login.clone(),
        display_name: e.display_name.clone(),
        tickets: e.tickets,
    }
}

/// Subscribers and members (founders count as subscribers).
pub fn is_supporter(actor: &Actor) -> bool {
    [Roles::SUBSCRIBER, Roles::MEMBER, Roles::FOUNDER].iter().any(|r| actor.roles.contains(*r))
}

impl Giveaway {
    pub fn start(id: String, spec: GiveawaySpec, now: Millis) -> Giveaway {
        Giveaway {
            id,
            prize: spec.prize,
            keyword: spec.keyword,
            cost: spec.cost,
            subscriber_bonus: spec.subscriber_bonus,
            started_at: now,
            open: true,
            ended: false,
            entrants: Vec::new(),
            winners: Vec::new(),
            keys: HashSet::new(),
        }
    }

    /// Whether `text` is this giveaway's keyword (the first word, any case).
    pub fn is_keyword(&self, text: &str) -> bool {
        crate::command::parse(text).is_some_and(|i| i.word.eq_ignore_ascii_case(&self.keyword))
    }

    /// Checks whether `actor` (person `key`) may enter, without entering them. The engine
    /// asks this before taking points, then calls [`Giveaway::enter`].
    pub fn may_enter(&self, key: &str, actor: &Actor) -> Result<(), NotEntered> {
        if !self.open || self.ended {
            return Err(NotEntered::Closed);
        }
        if !is_audience(actor) {
            return Err(NotEntered::NotAllowed);
        }
        if self.keys.contains(key) {
            return Err(NotEntered::Already);
        }
        if self.entrants.len() >= MAX_ENTRANTS {
            return Err(NotEntered::Full);
        }
        Ok(())
    }

    pub fn enter(&mut self, key: &str, actor: &Actor, paid: i64) -> Result<&Entrant, NotEntered> {
        self.may_enter(key, actor)?;
        let Some(user_id) = actor.id.clone() else { return Err(NotEntered::NotAllowed) };
        let tickets = if self.subscriber_bonus && is_supporter(actor) { 2 } else { 1 };
        self.keys.insert(key.to_string());
        self.entrants.push(Entrant {
            platform: actor.platform,
            user_id,
            login: actor.login.clone().unwrap_or_else(|| actor.display_name.clone()),
            display_name: actor.display_name.clone(),
            tickets,
            paid,
            key: key.to_string(),
        });
        Ok(&self.entrants[self.entrants.len() - 1])
    }

    /// Draws one more winner from those who haven't won yet (closing entries first).
    /// `Ok(None)` when nobody is left. `rand` must give uniform `u64`s.
    pub fn draw<E>(&mut self, rand: impl FnMut() -> Result<u64, E>) -> Result<Option<Entrant>, E> {
        self.open = false;
        let won: HashSet<&str> = self.winners.iter().map(|w| w.key.as_str()).collect();
        let pool: Vec<&Entrant> = self.entrants.iter().filter(|e| !won.contains(e.key.as_str())).collect();
        let weights: Vec<u64> = pool.iter().map(|e| u64::from(e.tickets)).collect();
        let Some(i) = pick_weighted(&weights, rand)? else { return Ok(None) };
        let winner = pool[i].clone();
        self.winners.push(winner.clone());
        Ok(Some(winner))
    }

    pub fn view(&self) -> GiveawayView {
        GiveawayView {
            id: self.id.clone(),
            prize: self.prize.clone(),
            keyword: self.keyword.clone(),
            cost: self.cost,
            subscriber_bonus: self.subscriber_bonus,
            started_at: self.started_at,
            open: self.open,
            ended: self.ended,
            entries: self.entrants.len() as u64,
            tickets: self.entrants.iter().map(|e| u64::from(e.tickets)).sum(),
            entrants: self.entrants.iter().rev().take(500).map(entrant_view).collect(),
            winners: self.winners.iter().map(entrant_view).collect(),
        }
    }

    pub fn card(&self) -> GiveawayCard {
        GiveawayCard {
            id: self.id.clone(),
            prize: self.prize.clone(),
            keyword: self.keyword.clone(),
            open: self.open,
            entries: self.entrants.len() as u64,
            winner: self.winners.last().map(entrant_view),
        }
    }

    pub fn announcement(&self) -> String {
        let mut s = format!("Giveaway: {}! Type {} to enter.", self.prize, self.keyword);
        if self.cost > 0 {
            s.push_str(&format!(" It costs {} points.", crate::loyalty::group(self.cost)));
        }
        if self.subscriber_bonus {
            s.push_str(" Subscribers get 2 entries.");
        }
        s
    }

    pub fn winner_message(&self, w: &Entrant) -> String {
        format!("Congratulations @{}, you won {}!", w.display_name, self.prize)
    }
}

/// Picks an index with probability proportional to its weight, from uniform random `u64`s,
/// with rejection sampling so no index is favoured by modulo bias. `None` for no weight.
pub fn pick_weighted<E>(weights: &[u64], mut rand: impl FnMut() -> Result<u64, E>) -> Result<Option<usize>, E> {
    let total: u64 = weights.iter().fold(0u64, |a, w| a.saturating_add(*w));
    if total == 0 {
        return Ok(None);
    }
    // The largest multiple of `total` that fits: values at or above it are redrawn.
    let zone = u64::MAX - (u64::MAX % total);
    let r = loop {
        let r = rand()?;
        if r < zone {
            break r % total;
        }
    };
    let mut acc = 0u64;
    for (i, w) in weights.iter().enumerate() {
        acc = acc.saturating_add(*w);
        if r < acc {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> GiveawaySpec {
        GiveawaySpec { prize: "  A  game key ".into(), keyword: "!JOIN".into(), cost: 0, subscriber_bonus: true }
    }

    fn viewer(name: &str, roles: Roles) -> Actor {
        let mut a = Actor::named(Platform::Twitch, name);
        a.id = Some(format!("{name}-id"));
        a.roles = roles;
        a
    }

    #[test]
    fn specs_are_cleaned_and_checked() {
        let c = spec().cleaned().unwrap();
        assert_eq!((c.prize.as_str(), c.keyword.as_str()), ("A game key", "!join"));
        let err = |s: GiveawaySpec| s.cleaned().unwrap_err();
        assert!(err(GiveawaySpec { prize: " ".into(), ..spec() }).starts_with("prize:"));
        assert!(err(GiveawaySpec { prize: "x".repeat(101), ..spec() }).starts_with("prize:"));
        assert!(err(GiveawaySpec { keyword: "join".into(), ..spec() }).starts_with("keyword:"));
        assert!(err(GiveawaySpec { keyword: "!join now".into(), ..spec() }).starts_with("keyword:"));
        assert!(err(GiveawaySpec { keyword: "!".into(), ..spec() }).starts_with("keyword:"));
        assert!(err(GiveawaySpec { cost: -1, ..spec() }).starts_with("cost:"));
        assert!(err(GiveawaySpec { cost: MAX_ADJUST + 1, ..spec() }).starts_with("cost:"));
    }

    #[test]
    fn entries_once_per_person_never_the_streamer_or_bots_and_subs_get_two() {
        let mut g = Giveaway::start("g".into(), spec().cleaned().unwrap(), Millis(0));
        assert!(g.is_keyword("!Join please"));
        assert!(!g.is_keyword("I want to !join"));
        assert_eq!(g.enter("p1", &viewer("ana", Roles::empty()), 0).unwrap().tickets, 1);
        assert_eq!(g.enter("p2", &viewer("sub", Roles::SUBSCRIBER), 0).unwrap().tickets, 2);
        assert_eq!(g.enter("p1", &viewer("ana_on_kick", Roles::empty()), 0).err(), Some(NotEntered::Already));
        assert_eq!(g.enter("p3", &viewer("me", Roles::BROADCASTER), 0).err(), Some(NotEntered::NotAllowed));
        assert_eq!(g.enter("p4", &viewer("bot", Roles::BOT), 0).err(), Some(NotEntered::NotAllowed));
        assert_eq!(g.enter("p5", &viewer("StreamElements", Roles::empty()), 0).err(), Some(NotEntered::NotAllowed));
        let v = g.view();
        assert_eq!((v.entries, v.tickets), (2, 3));
        g.open = false;
        assert_eq!(g.enter("p6", &viewer("late", Roles::empty()), 0).err(), Some(NotEntered::Closed));
    }

    #[test]
    fn draws_never_repeat_a_winner_and_run_out_cleanly() {
        let mut g = Giveaway::start("g".into(), spec().cleaned().unwrap(), Millis(0));
        g.enter("a", &viewer("a", Roles::empty()), 0).unwrap();
        g.enter("b", &viewer("b", Roles::empty()), 0).unwrap();
        let mut n = 0u64;
        let mut rand = || -> Result<u64, ()> {
            n += 1;
            Ok(n)
        };
        let first = g.draw(&mut rand).unwrap().unwrap();
        assert!(!g.open, "drawing closes entries");
        let second = g.draw(&mut rand).unwrap().unwrap();
        assert_ne!(first.key, second.key);
        assert_eq!(g.draw(&mut rand).unwrap(), None, "everyone has won");
        assert_eq!(g.card().winner.unwrap().user_id, second.user_id);
        let failing = || -> Result<u64, &str> { Err("no entropy") };
        let mut empty = Giveaway::start("e".into(), spec(), Millis(0));
        assert_eq!(empty.draw(failing).unwrap(), None, "nothing to draw needs no randomness");
        empty.open = true;
        empty.enter("x", &viewer("x", Roles::empty()), 0).unwrap();
        assert_eq!(empty.draw(failing), Err("no entropy"));
    }

    #[test]
    fn weighted_picks_are_fair_and_reject_biased_values() {
        // Every r in 0..total maps to its slot: weights 1 and 2 → slot 0 once, slot 1 twice.
        let picks: Vec<usize> = (0..3u64).map(|r| pick_weighted::<()>(&[1, 2], || Ok(r)).unwrap().unwrap()).collect();
        assert_eq!(picks, vec![0, 1, 1]);
        // A value in the biased zone at the top is thrown away and the next one is used.
        let mut values = vec![u64::MAX, 3].into_iter();
        assert_eq!(pick_weighted::<()>(&[1, 2], || Ok(values.next().unwrap())).unwrap(), Some(0));
        assert_eq!(pick_weighted::<()>(&[], || Ok(0)).unwrap(), None);
        assert_eq!(pick_weighted::<()>(&[0, 0], || Ok(0)).unwrap(), None);
    }

    #[test]
    fn messages() {
        let mut s = spec().cleaned().unwrap();
        s.cost = 1_500;
        let g = Giveaway::start("g".into(), s, Millis(0));
        assert_eq!(
            g.announcement(),
            "Giveaway: A game key! Type !join to enter. It costs 1,500 points. Subscribers get 2 entries."
        );
    }
}
