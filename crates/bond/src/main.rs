//! The Bond service: `ferrocast-bond`, set up from the environment.
//!
//!   BOND_SECRET     the secret Bond tokens are signed with (the relay has the same); required
//!   BOND_HTTP       where the API listens (default 0.0.0.0:8795)
//!   BOND_SRTLA      where SRTLA arrives, UDP (default 0.0.0.0:5000)
//!   BOND_SRT        where SRT arrives, UDP (default 0.0.0.0:5001)
//!   BOND_KEEPALIVE  minutes Be right back holds a quiet stream (default 10, at most 60)
//!   BOND_MAX        streams at once (default 50)
//!   BOND_FFMPEG     ffmpeg for the Be right back picture (default: ffmpeg on the PATH)

use std::time::Duration;

#[tokio::main]
async fn main() -> std::io::Result<()> {
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
        allow_private: false,
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
