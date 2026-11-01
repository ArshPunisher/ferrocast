//! The Bond service: `ferrocast-bond`, set up from the environment.
//!
//!   BOND_SECRET     the secret Bond tokens are signed with (the relay has the same); required
//!   BOND_HTTP       where the API listens (default 0.0.0.0:8795)
//!   BOND_SRTLA      where SRTLA arrives, UDP (default 0.0.0.0:5000)
//!   BOND_SRT        where SRT arrives, UDP (default 0.0.0.0:5001)
//!   BOND_KEEPALIVE  minutes Be right back holds a quiet stream (default 10, at most 60)
//!   BOND_MAX        streams at once (default 50)
//!   BOND_FFMPEG     ffmpeg for the Be right back picture and phones (default: ffmpeg on the PATH)
//!   BOND_PUBLIC_URL the service's HTTPS address, for phones' links (default http://127.0.0.1:8795)
//!   BOND_RTC_IP     the address phones' WebRTC arrives at (default: the one traffic leaves by)
//!   BOND_RTC_PORTS  UDP ports open for it (default 40000-40099)
//!   BOND_ALLOW_PRIVATE=1  platforms on private networks too (testing on one computer only)
//!   BOND_VAPID_KEY  the key phone pushes are signed with (`ferrocast-bond vapid-key` makes one)

use std::time::Duration;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("vapid-key") {
        println!("{}", ferrocast_bond::push::new_key().ok_or_else(|| std::io::Error::other("no key made"))?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("BOND_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let addr = |k: &str, d: &str| env(k, d).parse().map_err(|e| std::io::Error::other(format!("{k}: {e}")));
    let secret = std::env::var("BOND_SECRET").unwrap_or_default();
    if secret.len() < 32 {
        return Err(std::io::Error::other("BOND_SECRET: at least 32 characters (the relay has the same one)"));
    }
    let ffmpeg = std::path::PathBuf::from(env("BOND_FFMPEG", "ffmpeg"));
    let has_ffmpeg = std::process::Command::new(&ffmpeg).arg("-version").output().is_ok_and(|o| o.status.success());
    if !has_ffmpeg {
        tracing::warn!("no ffmpeg: streams end when their computer drops (no Be right back picture)");
    }
    let cfg = ferrocast_bond::Config {
        secret: secret.into_bytes(),
        srtla: addr("BOND_SRTLA", "0.0.0.0:5000")?,
        srt: addr("BOND_SRT", "0.0.0.0:5001")?,
        keepalive: Duration::from_secs(60 * env("BOND_KEEPALIVE", "10").parse::<u64>().unwrap_or(10).clamp(1, 60)),
        max_sessions: env("BOND_MAX", "50").parse().unwrap_or(50),
        ffmpeg: has_ffmpeg.then_some(ffmpeg),
        allow_private: std::env::var("BOND_ALLOW_PRIVATE").is_ok_and(|v| v == "1"),
        public_url: env("BOND_PUBLIC_URL", "http://127.0.0.1:8795").trim_end_matches('/').to_string(),
        rtc_ip: std::env::var("BOND_RTC_IP").ok().and_then(|v| v.parse().ok()),
        vapid: std::env::var("BOND_VAPID_KEY").ok().filter(|k| !k.trim().is_empty()),
        rtc_ports: {
            let r = env("BOND_RTC_PORTS", "40000-40099");
            let (a, b) = r.split_once('-').unwrap_or((r.as_str(), r.as_str()));
            (a.trim().parse().unwrap_or(40000), b.trim().parse().unwrap_or(40099))
        },
    };
    let http: std::net::SocketAddr = addr("BOND_HTTP", "0.0.0.0:8795")?;
    let bond = ferrocast_bond::Bond::new(cfg);
    let media = bond.clone();
    tokio::spawn(async move {
        if let Err(e) = media.run_media().await {
            tracing::error!(error = %e, "Bond: the media side stopped");
            std::process::exit(1);
        }
    });
    let listener = tokio::net::TcpListener::bind(http).await?;
    tracing::info!(%http, "Ferrocast Bond is up");
    axum::serve(listener, bond.router()).await
}
