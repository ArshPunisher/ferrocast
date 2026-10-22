//! A destination's server address (`rtmp://host[:port]/app` or `rtmps://…`), checked and split
//! into what the RTMP connect needs. The stream key is kept apart and never part of the address.

use serde::Serialize;

/// Longest server address we accept, as typed or fetched.
const MAX_URL: usize = 512;

/// A checked server address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// The RTMP application, everything after the host (`live2`, `app`, `live/abc`).
    pub app: String,
    /// The address as the server expects it in `connect` (no trailing slash).
    pub tc_url: String,
}

/// Why an address was refused, in words for the person who typed it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("The server address must start with rtmp:// or rtmps://.")]
    Scheme,
    #[error("The server address is missing its host name.")]
    NoHost,
    #[error("The server address is missing the part after the host (for example /live).")]
    NoApp,
    #[error("The server address has a port that isn't a number from 1 to 65535.")]
    Port,
    #[error("The server address can't contain a user name, spaces or a #.")]
    Characters,
    #[error("The server address is too long.")]
    TooLong,
}

impl Target {
    pub fn parse(url: &str) -> Result<Self, TargetError> {
        let url = url.trim();
        if url.len() > MAX_URL {
            return Err(TargetError::TooLong);
        }
        let lower = url.to_ascii_lowercase();
        let (tls, rest) = if lower.starts_with("rtmps://") {
            (true, &url[8..])
        } else if lower.starts_with("rtmp://") {
            (false, &url[7..])
        } else {
            return Err(TargetError::Scheme);
        };
        if rest.chars().any(|c| c.is_whitespace() || c.is_control() || c == '#' || c == '@') {
            return Err(TargetError::Characters);
        }
        let (authority, app) = rest.split_once('/').unwrap_or((rest, ""));
        let app = app.trim_matches('/');
        if app.is_empty() {
            return Err(TargetError::NoApp);
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p.parse().map_err(|_| TargetError::Port)?;
                if port == 0 {
                    return Err(TargetError::Port);
                }
                (h, port)
            }
            None => (authority, if tls { 443 } else { 1935 }),
        };
        if host.is_empty() || host.starts_with('[') {
            return Err(TargetError::NoHost);
        }
        let host = host.to_ascii_lowercase();
        let scheme = if tls { "rtmps" } else { "rtmp" };
        let tc_url = format!("{scheme}://{authority}/{app}");
        Ok(Self { tls, host, port, app: app.to_string(), tc_url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_addresses() {
        let t = Target::parse("rtmp://live.twitch.tv/app").unwrap();
        assert_eq!((t.tls, t.host.as_str(), t.port, t.app.as_str()), (false, "live.twitch.tv", 1935, "app"));
        let y = Target::parse("rtmps://a.rtmps.youtube.com/live2/").unwrap();
        assert_eq!((y.tls, y.port, y.app.as_str()), (true, 443, "live2"));
        assert_eq!(y.tc_url, "rtmps://a.rtmps.youtube.com/live2");
        let k = Target::parse("rtmps://fa723fc1b171.global-contribute.live-video.net:443/app/").unwrap();
        assert_eq!((k.port, k.app.as_str()), (443, "app"));
        let l = Target::parse("  RTMP://127.0.0.1:19350/live/extra  ").unwrap();
        assert_eq!((l.host.as_str(), l.port, l.app.as_str()), ("127.0.0.1", 19350, "live/extra"));
    }

    #[test]
    fn refuses_bad_addresses() {
        assert_eq!(Target::parse("https://example.com/live"), Err(TargetError::Scheme));
        assert_eq!(Target::parse("example.com/live"), Err(TargetError::Scheme));
        assert_eq!(Target::parse("rtmp://example.com"), Err(TargetError::NoApp));
        assert_eq!(Target::parse("rtmp://example.com///"), Err(TargetError::NoApp));
        assert_eq!(Target::parse("rtmp:///live"), Err(TargetError::NoHost));
        assert_eq!(Target::parse("rtmp://example.com:0/live"), Err(TargetError::Port));
        assert_eq!(Target::parse("rtmp://example.com:99999/live"), Err(TargetError::Port));
        assert_eq!(Target::parse("rtmp://example.com:x/live"), Err(TargetError::Port));
        assert_eq!(Target::parse("rtmp://user:pw@example.com/live"), Err(TargetError::Characters));
        assert_eq!(Target::parse("rtmp://exa mple.com/live"), Err(TargetError::Characters));
        assert_eq!(Target::parse("rtmp://example.com/live#x"), Err(TargetError::Characters));
        assert_eq!(Target::parse("rtmp://[::1]:1935/live"), Err(TargetError::NoHost));
        let long = format!("rtmp://example.com/{}", "a".repeat(600));
        assert_eq!(Target::parse(&long), Err(TargetError::TooLong));
        assert_eq!(Target::parse(""), Err(TargetError::Scheme));
        assert_eq!(Target::parse("rtmp://例え.jp/live").map(|t| t.port), Ok(1935));
    }
}
