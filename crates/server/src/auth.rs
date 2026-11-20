//! Who may call the dashboard API (API.md §2, R-43).
//!
//! The server only listens on 127.0.0.1, but a web page in the streamer's own browser can
//! still send requests there, and DNS rebinding can make them look same-origin. So every
//! API call needs a session token, the `Host` header must name this server, and a present
//! `Origin` must be our own.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
use subtle::ConstantTimeEq;

use crate::error::ApiError;

/// Pairing codes live this long.
pub const PAIR_TTL: Duration = Duration::from_secs(120);
/// More failed pairings than this in a minute locks pairing for that minute.
const MAX_PAIR_FAILURES: usize = 10;
/// Maximum paired browser/phone session tokens kept alongside the desktop token (oldest evicted).
const MAX_PAIRED_TOKENS: usize = 64;
/// Maximum outstanding unredeemed pairing codes at once.
const MAX_PENDING_CODES: usize = 64;
/// Alphabet without look-alikes (no 0/O, 1/I/L) so codes can be typed from a phone.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

#[derive(Debug)]
pub struct Sessions {
    /// Paired browser/phone tokens (oldest evicted at [`MAX_PAIRED_TOKENS`]); the desktop token
    /// is checked directly and never evicted.
    tokens: Mutex<VecDeque<String>>,
    codes: Mutex<HashMap<String, Instant>>,
    failures: Mutex<VecDeque<Instant>>,
    desktop: String,
}

impl Sessions {
    pub fn new() -> Self {
        let desktop = random_hex(32);
        Sessions {
            tokens: Mutex::new(VecDeque::new()),
            codes: Mutex::new(HashMap::new()),
            failures: Mutex::new(VecDeque::new()),
            desktop,
        }
    }

    /// The token the desktop window gets injected.
    pub fn desktop_token(&self) -> &str {
        &self.desktop
    }

    /// Constant-time check so response timing doesn't leak how much of a token matched.
    pub fn valid(&self, token: &str) -> bool {
        if bool::from(self.desktop.as_bytes().ct_eq(token.as_bytes())) {
            return true;
        }
        let tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.iter().any(|t| t.as_bytes().ct_eq(token.as_bytes()).into())
    }

    /// A fresh one-time pairing code.
    pub fn new_code(&self) -> String {
        let mut bytes = [0u8; 10];
        fill(&mut bytes);
        let code: String = bytes.iter().map(|b| CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()] as char).collect();
        let mut codes = self.codes.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        codes.retain(|_, at| now.duration_since(*at) < PAIR_TTL);
        while codes.len() >= MAX_PENDING_CODES {
            let Some(oldest) = codes.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()) else {
                break;
            };
            codes.remove(&oldest);
        }
        codes.insert(code.clone(), now);
        code
    }

    /// Trades a pairing code for a session token.
    pub fn pair(&self, code: &str) -> Result<String, ApiError> {
        let now = Instant::now();
        {
            let mut f = self.failures.lock().unwrap_or_else(|p| p.into_inner());
            while f.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                f.pop_front();
            }
            if f.len() >= MAX_PAIR_FAILURES {
                return Err(ApiError::too_many(60));
            }
        }
        let code = code.trim().to_ascii_uppercase();
        let ok = {
            let mut codes = self.codes.lock().unwrap_or_else(|p| p.into_inner());
            match codes.remove(&code) {
                Some(at) => now.duration_since(at) < PAIR_TTL,
                None => false,
            }
        };
        if !ok {
            self.failures.lock().unwrap_or_else(|p| p.into_inner()).push_back(now);
            return Err(ApiError::new(
                axum::http::StatusCode::UNAUTHORIZED,
                "bad_code",
                "That code is wrong or expired.",
            )
            .hint("Make a new link in the app: Settings → Open in browser."));
        }
        let token = random_hex(32);
        let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        while tokens.len() >= MAX_PAIRED_TOKENS {
            tokens.pop_front();
        }
        tokens.push_back(token.clone());
        Ok(token)
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Origin and Host rules for one server.
#[derive(Debug, Clone)]
pub struct Guard {
    pub port: u16,
    /// LAN access turned on: any Host is accepted (the token is still required).
    pub lan: bool,
    /// Extra allowed origins for frontend development (`FERROCAST_DEV_ORIGIN`).
    pub dev_origins: Vec<String>,
}

impl Guard {
    pub fn host_ok(&self, headers: &HeaderMap) -> bool {
        if self.lan {
            return true;
        }
        let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
            return false;
        };
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    /// `true` when there's no Origin (same-origin GETs, OBS, curl) or it's one of ours.
    pub fn origin_ok(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN).and_then(|h| h.to_str().ok()) else {
            return true;
        };
        origin == format!("http://127.0.0.1:{}", self.port)
            || origin == format!("http://localhost:{}", self.port)
            || self.dev_origins.iter().any(|d| d == origin)
            || (self.lan && origin.starts_with("http://") && !origin.contains('@'))
    }
}

/// The bearer token from `Authorization`, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

fn fill(buf: &mut [u8]) {
    // Only fails when the OS has no randomness at all; nothing safe is possible then.
    #[allow(clippy::expect_used)]
    getrandom::fill(buf).expect("the OS random number generator is unavailable");
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(axum::http::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn pairing_codes_are_one_time() {
        let s = Sessions::new();
        let code = s.new_code();
        assert_eq!(code.len(), 10);
        let token = s.pair(&code.to_lowercase()).unwrap();
        assert!(s.valid(&token));
        assert!(s.pair(&code).is_err(), "second use fails");
        assert!(!s.valid("nope"));
        assert!(s.valid(s.desktop_token()));
    }

    #[test]
    fn brute_force_is_locked_out() {
        let s = Sessions::new();
        let good = s.new_code();
        for _ in 0..MAX_PAIR_FAILURES {
            assert_eq!(s.pair("WRONGWRONG").unwrap_err().status, axum::http::StatusCode::UNAUTHORIZED);
        }
        assert_eq!(s.pair(&good).unwrap_err().status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn host_and_origin_rules() {
        let g = Guard { port: 47820, lan: false, dev_origins: vec!["http://localhost:5173".into()] };
        assert!(g.host_ok(&headers(&[(HOST, "127.0.0.1:47820")])));
        assert!(g.host_ok(&headers(&[(HOST, "localhost:47820")])));
        assert!(!g.host_ok(&headers(&[(HOST, "evil.example:47820")])), "DNS rebinding");
        assert!(!g.host_ok(&HeaderMap::new()));
        assert!(g.origin_ok(&HeaderMap::new()));
        assert!(g.origin_ok(&headers(&[(ORIGIN, "http://127.0.0.1:47820")])));
        assert!(g.origin_ok(&headers(&[(ORIGIN, "http://localhost:5173")])));
        assert!(!g.origin_ok(&headers(&[(ORIGIN, "https://evil.example")])));
        assert!(!g.origin_ok(&headers(&[(ORIGIN, "http://127.0.0.1:9999")])));
        assert!(!g.origin_ok(&headers(&[(ORIGIN, "null")])));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer(&headers(&[(AUTHORIZATION, "Bearer abc")])), Some("abc"));
        assert_eq!(bearer(&headers(&[(AUTHORIZATION, "Basic abc")])), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn sessions_and_codes_stay_capped_and_keep_desktop_token() {
        let s = Sessions::new();
        for _ in 0..200 {
            let _ = s.new_code();
        }
        assert!(s.codes.lock().unwrap().len() <= MAX_PENDING_CODES);
        let mut issued = Vec::new();
        for _ in 0..200 {
            let code = s.new_code();
            issued.push(s.pair(&code).unwrap());
        }
        assert_eq!(s.tokens.lock().unwrap().len(), MAX_PAIRED_TOKENS);
        assert!(s.valid(s.desktop_token()), "desktop token is never evicted");
        assert!(!s.valid(&issued[0]), "oldest paired token was evicted");
        assert!(s.valid(issued.last().unwrap()), "newest paired token remains valid");
    }
}
