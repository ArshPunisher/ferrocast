//! `ferrocast-relay`: run with `RELAY_BIND`, `RELAY_DATA` (and optionally `RELAY_BLOB_DIR`,
//! `RELAY_VAULT_KEY`, `GOOGLE_TTS_KEY`) and the settings in `deploy/relay.env.example`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ferrocast_relay::billing::{self, BillingConfig};
use ferrocast_relay::blob::{BlobStore, FsBlobStore};
use ferrocast_relay::vault::Vault;
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
    let billing_cfg = BillingConfig::from_env();
    let complete = billing_cfg.webhook_secret.is_some()
        && billing_cfg.email_key.is_some()
        && billing_cfg.plan_monthly.is_some()
        && billing_cfg.plan_yearly.is_some();
    match (billing_cfg.mode(), complete) {
        (Some(mode), true) => {
            tracing::info!(mode, sales_open = billing_cfg.sales_open, "Pro billing is set up")
        }
        _ => tracing::warn!("Razorpay keys, webhook secret, PRO_EMAIL_KEY or plan ids not set: Pro billing is off"),
    }
    // Pro's shared parts. Missing ones only switch off the features that need them.
    let vault = match Vault::from_env() {
        Ok(Some(v)) => Some(v),
        Ok(None) => {
            tracing::warn!("RELAY_VAULT_KEY not set: Pro features that keep secrets are off");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "RELAY_VAULT_KEY is unusable: Pro features that keep secrets are off");
            None
        }
    };
    let blob_dir = FsBlobStore::default_root(&data);
    let blobs: Option<Arc<dyn BlobStore>> = match FsBlobStore::new(&blob_dir) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::error!(error = %e, dir = %blob_dir.display(), "can't use the blob folder: Pro file storage is off");
            None
        }
    };
    let tts_cap = ferrocast_relay::tts::global_cap_from_env();
    let tts: Option<Arc<dyn ferrocast_relay::tts::TtsProvider>> = match ferrocast_relay::tts::GoogleTts::from_env() {
        Some(g) => {
            tracing::info!(monthly_cap = tts_cap, "Ferrocast voices are on (Google Cloud TTS)");
            Some(Arc::new(g))
        }
        None => {
            tracing::warn!("GOOGLE_TTS_KEY not set: Ferrocast voices are off");
            None
        }
    };
    let state =
        AppState::new(cfg, db).with_billing(billing_cfg).with_vault(vault).with_blobs(blobs).with_tts(tts, tts_cap);
    let daily = state.clone();
    tokio::spawn(async move {
        // The safety net for lost webhooks, a while after start and then daily.
        tokio::time::sleep(Duration::from_secs(600)).await;
        loop {
            let n = billing::reconcile(&daily).await;
            tracing::info!(subscriptions = n, "Pro daily re-check done");
            tokio::time::sleep(Duration::from_secs(86_400)).await;
        }
    });
    let housekeeping = state.db.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = housekeeping.prune() {
                tracing::warn!(error = %e, "housekeeping failed");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let tips = state.clone();
    tokio::spawn(async move {
        // Pro tip pages: ended goals and lapsed installs' branding (tips::sweep).
        loop {
            ferrocast_relay::tips::sweep(&tips).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let backups = state.clone();
    tokio::spawn(async move {
        // Pro cloud backups: unfinished uploads after a day, lapsed subscriptions' after 30 days.
        loop {
            ferrocast_relay::backups::sweep(&backups).await;
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
