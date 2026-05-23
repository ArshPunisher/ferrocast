//! `ferrocast-relay`: run with `RELAY_BIND`, `RELAY_DATA` and the settings in
//! `deploy/relay.env.example`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ferrocast_relay::{AppState, Config, Db, router};

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    let bind: SocketAddr =
        std::env::var("RELAY_BIND").ok().and_then(|b| b.parse().ok()).unwrap_or_else(|| ([127, 0, 0, 1], 8787).into());
    let data = PathBuf::from(std::env::var("RELAY_DATA").unwrap_or_else(|_| "relay.db".into()));
    let db = match Db::open(&data) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("ferrocast-relay: can't open {}: {e}", data.display());
            std::process::exit(1);
        }
    };
    let cfg = Config::from_env();
    if cfg.kick_client_id.is_none() || cfg.kick_client_secret.is_none() {
        tracing::warn!("KICK_CLIENT_ID / KICK_CLIENT_SECRET not set: Kick sign-in is off");
    }
    let state = AppState::new(cfg, db);
    let housekeeping = state.db.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = housekeeping.prune() {
                tracing::warn!(error = %e, "housekeeping failed");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ferrocast-relay: can't listen on {bind}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(%bind, "relay listening");
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("ferrocast-relay: stopped: {e}");
        std::process::exit(1);
    }
}
