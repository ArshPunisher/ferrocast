//! Text from a web address (the OBS URL/API source plugin, built in): every so often the address
//! is read, one value picked out of its JSON (`data.viewers`, `items.0.name`) or the whole reply
//! taken as text, put in the words ("Viewers: {}") and drawn like a countdown, only when it
//! changes. The engine has no web client: the app hands one in (`set_fetcher`), with its own
//! limits on time and size. Until the first reply it's clear; a failed read keeps what it showed.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;

use crate::engine::Feed;
use crate::source::series::{Worker, spawn, wait};
use crate::source::text::{TextRenderer, TextStyle};

#[derive(Debug, Clone, PartialEq)]
pub struct WebText {
    /// An http or https address.
    pub url: String,
    /// The value to show, as a path into the reply's JSON (`a.b.0.c`); empty: the whole reply.
    pub path: String,
    /// The words, with `{}` where the value goes (no `{}`: the value after them).
    pub words: String,
    /// Seconds between reads (5 to 3600).
    pub every_s: u64,
    pub style: TextStyle,
}

/// Reads an address and gives back its reply as text, or why not.
pub type Fetcher = fn(&str) -> Result<String, String>;

static FETCHER: RwLock<Option<Fetcher>> = RwLock::new(None);

/// The app's web client, for every Web text source.
pub fn set_fetcher(f: Fetcher) {
    *FETCHER.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

fn fetch(url: &str) -> Result<String, String> {
    let f = *FETCHER.read().unwrap_or_else(|e| e.into_inner());
    f.ok_or_else(|| "this app can't read web addresses".to_string())?(url)
}

/// Whether an address answers (a page a Browser source would show): Ok, or why not ("404",
/// "couldn't reach it"). Through the app's web client, with its limits.
pub fn check(url: &str) -> Result<(), String> {
    fetch(url).map(|_| ())
}

/// The last read of a Web text: when (ms since 1970), the value it showed, and why the last
/// read failed (None when it worked).
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub at_ms: u64,
    pub value: Option<String>,
    pub error: Option<String>,
}

/// The last read of each address and path being read.
static STATUS: Mutex<Option<std::collections::HashMap<(String, String), Status>>> = Mutex::new(None);

/// How the last read of `url` at `path` went, if it's been read.
pub fn status(url: &str, path: &str) -> Option<Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(&(url.to_string(), path.to_string())).cloned()
}

fn note(url: &str, path: &str, got: &Result<String, String>) {
    let at_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut all = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    let map = all.get_or_insert_with(Default::default);
    let key = (url.to_string(), path.to_string());
    // A failed read keeps the value it last showed.
    let value = match got {
        Ok(v) => Some(v.clone()),
        Err(_) => map.get(&key).and_then(|s| s.value.clone()),
    };
    map.insert(key, Status { at_ms, value, error: got.as_ref().err().cloned() });
}

/// The most a value shows (characters), so a whole web page can't fill the screen.
const MOST: usize = 300;

/// The value at `path` in `reply` (JSON), or the reply itself when the path is empty. None when
/// the path leads nowhere or the reply isn't JSON but a path was asked for.
pub fn pick(reply: &str, path: &str) -> Option<String> {
    let path = path.trim().trim_start_matches('$').trim_start_matches('.');
    let out = if path.is_empty() {
        // JSON text ("hi") shows without its quotes; anything else as it came.
        match serde_json::from_str::<Value>(reply) {
            Ok(Value::String(s)) => s,
            _ => reply.trim().to_string(),
        }
    } else {
        let mut at = serde_json::from_str::<Value>(reply).ok()?;
        for part in path.split('.') {
            at = match at {
                Value::Object(mut m) => m.remove(part)?,
                Value::Array(mut a) => {
                    let i: usize = part.trim_matches(|c| c == '[' || c == ']').parse().ok()?;
                    (i < a.len()).then(|| a.swap_remove(i))?
                }
                _ => return None,
            };
        }
        match at {
            Value::String(s) => s,
            Value::Null => String::new(),
            v => v.to_string(),
        }
    };
    Some(out.chars().take(MOST).collect())
}

/// The words with the value in them.
pub fn fill(words: &str, value: &str) -> String {
    if words.contains("{}") { words.replacen("{}", value, 1) } else { format!("{words}{value}") }
}

/// Reads and draws into `feed` until stopped, with the engine's `text` renderer.
pub(crate) fn web_text(w: WebText, feed: Feed, text: Arc<Mutex<TextRenderer>>) -> Worker {
    spawn("ferrocast-web-text", move |stop: &AtomicBool| {
        let every = Duration::from_secs(w.every_s.clamp(5, 3600));
        let mut shown: Option<String> = None;
        let mut said_why = false;
        loop {
            // Read on a thread of its own: closing the source never waits for a slow address
            // (the client's own time limit ends that thread).
            let (tx, rx) = std::sync::mpsc::channel();
            let url = w.url.clone();
            std::thread::spawn(move || {
                let _ = tx.send(fetch(&url));
            });
            let got = loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(r) => break r,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if !stop.load(std::sync::atomic::Ordering::Acquire) => {}
                    Err(_) => return,
                }
            };
            let got = got.and_then(|r| pick(&r, &w.path).ok_or_else(|| format!("nothing at \"{}\"", w.path)));
            note(&w.url, &w.path, &got);
            match got {
                Ok(v) => {
                    said_why = false;
                    let now = fill(&w.words, &v);
                    if shown.as_deref() != Some(now.as_str()) {
                        let p = if now.trim().is_empty() {
                            crate::Picture::solid(1, 1, [0, 0, 0, 0])
                        } else {
                            text.lock().unwrap_or_else(|e| e.into_inner()).render(&now, &w.style)
                        };
                        feed.picture(p);
                        shown = Some(now);
                    }
                }
                // Once per run of failures, not every read.
                Err(e) if !said_why => {
                    tracing::warn!(url = %w.url, "a web text source couldn't read its address: {e}");
                    said_why = true;
                }
                Err(_) => {}
            }
            if !wait(stop, every) {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_picked_out_of_the_reply() {
        let r = r#"{"data":{"viewers":42,"title":"Hi","live":true,"none":null},"items":[{"name":"a"},{"name":"b"}]}"#;
        assert_eq!(pick(r, "data.viewers").as_deref(), Some("42"));
        assert_eq!(pick(r, "data.title").as_deref(), Some("Hi"), "text without quotes");
        assert_eq!(pick(r, "$.data.live").as_deref(), Some("true"), "a JSONPath-style start is fine");
        assert_eq!(pick(r, "items.1.name").as_deref(), Some("b"));
        assert_eq!(pick(r, "items.[0].name").as_deref(), Some("a"));
        assert_eq!(pick(r, "data.none").as_deref(), Some(""));
        // Leads nowhere.
        assert_eq!(pick(r, "data.nope"), None);
        assert_eq!(pick(r, "items.9.name"), None);
        assert_eq!(pick(r, "items.x"), None);
        assert_eq!(pick(r, "data.viewers.deeper"), None);
        assert_eq!(pick("not json", "a.b"), None);
        // No path: the whole reply.
        assert_eq!(pick("  Hello there \n", "").as_deref(), Some("Hello there"));
        assert_eq!(pick("\"quoted\"", "").as_deref(), Some("quoted"));
        // Long replies are cut.
        assert_eq!(pick(&"x".repeat(10_000), "").map(|s| s.chars().count()), Some(MOST));
    }

    #[test]
    fn the_value_goes_in_the_words() {
        assert_eq!(fill("Viewers: {}", "42"), "Viewers: 42");
        assert_eq!(fill("{} watching, {} max", "3"), "3 watching, {} max", "the first {{}} only");
        assert_eq!(fill("Song: ", "Blue"), "Song: Blue");
        assert_eq!(fill("", "Blue"), "Blue");
    }

    fn reply(url: &str) -> Result<String, String> {
        match url {
            "https://ok.test/" => Ok(r#"{"n":7}"#.into()),
            _ => Err("no such host".into()),
        }
    }

    /// The worker draws the value at once; an address that fails draws nothing.
    #[test]
    fn the_worker_draws_what_it_reads() {
        set_fetcher(reply);
        let fonts = Arc::new(Mutex::new(TextRenderer::new()));
        let w = |url: &str| WebText {
            url: url.into(),
            path: "n".into(),
            words: "Count: {}".into(),
            every_s: 5,
            style: TextStyle::default(),
        };
        let feed = Feed::default();
        let worker = web_text(w("https://ok.test/"), feed.clone(), fonts.clone());
        std::thread::sleep(Duration::from_millis(300));
        assert!(feed.take_picture().is_some_and(|p| p.width > 1), "the value drawn");
        drop(worker);
        let bad = Feed::default();
        let worker = web_text(w("https://down.test/"), bad.clone(), fonts);
        std::thread::sleep(Duration::from_millis(300));
        assert!(bad.take_picture().is_none(), "nothing read, nothing drawn (it stays clear)");
        drop(worker);
        // How each went, for the drawer's "Read 12 s ago" (the path here is "n").
        let ok = status("https://ok.test/", "n").unwrap_or_else(|| panic!("a read noted"));
        assert!(ok.at_ms > 0 && ok.error.is_none() && ok.value.is_some());
        let down = status("https://down.test/", "n").unwrap_or_else(|| panic!("a failed read noted"));
        assert!(down.error.is_some() && down.value.is_none());
    }
}
