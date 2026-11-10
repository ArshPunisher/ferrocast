//! The web client our engine's Web text sources read with (ferrocore has none of its own): web
//! addresses only, a few seconds and 64 KB at most, three redirects, and no cookies or sign-ins,
//! so a source reads what anyone on the web would see.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

/// The most of a reply read.
const MOST: u64 = 64 * 1024;

fn client() -> Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .connect_timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::custom(|a| {
                    if a.previous().len() >= 3 || !matches!(a.url().scheme(), "http" | "https") {
                        a.stop()
                    } else {
                        a.follow()
                    }
                }))
                .user_agent(concat!("Ferrocast/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Reads `url` as text (cut at 64 KB).
pub(crate) fn fetch(url: &str) -> Result<String, String> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err("only http and https addresses can be read".into());
    }
    let reply = client()?.get(url.trim()).send().map_err(|e| e.without_url().to_string())?;
    let status = reply.status();
    if !status.is_success() {
        return Err(format!("the address answered {status}"));
    }
    let mut body = Vec::new();
    reply.take(MOST).read_to_end(&mut body).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Hands our engine this client (once).
pub(crate) fn install() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| ferrocore::source::webtext::set_fetcher(fetch));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_addresses_are_read() {
        for bad in ["file:///etc/passwd", "ftp://x.test/", "", "  ", "data:text/plain,hi"] {
            assert!(fetch(bad).is_err(), "{bad:?}");
        }
    }

    /// A local server: the body is read, a long one is cut at 64 KB, and an error page fails.
    #[test]
    fn a_reply_is_read_and_cut() {
        use std::io::Write;
        let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0") else { return };
        let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
        std::thread::spawn(move || {
            for (i, s) in l.incoming().take(3).enumerate() {
                let Ok(mut s) = s else { continue };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let (status, body) = match i {
                    0 => ("200 OK", r#"{"n":7}"#.to_string()),
                    1 => ("200 OK", "x".repeat(200_000)),
                    _ => ("404 Not Found", "nope".to_string()),
                };
                let _ =
                    write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let url = format!("http://127.0.0.1:{port}/");
        assert_eq!(fetch(&url).as_deref(), Ok(r#"{"n":7}"#));
        assert_eq!(fetch(&url).map(|b| b.len()), Ok(MOST as usize), "cut at 64 KB");
        assert!(fetch(&url).is_err_and(|e| e.contains("404")), "an error page isn't a value");
    }
}
