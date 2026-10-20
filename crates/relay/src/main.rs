//! `ferrocast-relay`: run with `RELAY_BIND`, `RELAY_DATA` (and optionally `RELAY_BLOB_DIR`,
//! `RELAY_VAULT_KEY`, `GOOGLE_TTS_KEY`, `BREVO_API_KEY`, `RELAY_REMOTE_URL`, `RELAY_DASHBOARD_DIR`) and the settings in `deploy/relay.env.example`.
//!
//! `ferrocast-relay pro-grant <install-id> <days> | --revoke <install-id> | --list`: the
//! owner's test Pro (billing::comp) on the same `RELAY_DATA` database, then exits. Safe
//! while the server runs (WAL, busy timeout). Run on the server by `deploy/pro-grant.sh`.
//!
//! `ferrocast-relay bot-check [<install-id>]`: the owner's look at the Twitch cloud bot
//! (bot::check): the conduit, shard 0, EventSub subscriptions and each channel's row, with
//! no tokens or secrets, then exits. Run on the server by `deploy/bot-check.sh`.

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("pro-grant") {
        match billing::comp::cli(&db, &args[1..]) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("ferrocast-relay pro-grant: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.first().map(String::as_str) == Some("bot-check") {
        let install = match ferrocast_relay::bot::check::parse_args(&args[1..]) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("ferrocast-relay bot-check: {e}");
                std::process::exit(2);
            }
        };
        let cfg = Config::from_env();
        let api = match ferrocast_relay::bot::TwitchConfig::from_env() {
            Ok(Some(c)) => Some(ferrocast_relay::bot::HttpTwitch::new(&c)),
            Ok(None) => None,
            Err(e) => {
                eprintln!("ferrocast-relay bot-check: the Twitch cloud bot settings are incomplete: {e}");
                std::process::exit(1);
            }
        };
        let api = api.as_ref().map(|a| a as &dyn ferrocast_relay::bot::TwitchCloud);
        match ferrocast_relay::bot::check::report(&db, api, &cfg.public_url, install).await {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("ferrocast-relay bot-check: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
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
    let admin = ferrocast_relay::admin::AdminConfig::from_env(&data);
    match admin.state() {
        "on" => tracing::info!("the Control Center's admin stats are on"),
        "too_short" => tracing::error!(
            min = ferrocast_relay::admin::MIN_TOKEN_LEN,
            "RELAY_ADMIN_TOKEN is too short: the admin stats are off (run control-center-key.sh --rotate)"
        ),
        _ => tracing::warn!("RELAY_ADMIN_TOKEN not set: the Control Center's admin stats are off"),
    }
    let state = AppState::new(cfg, db)
        .with_billing(billing_cfg)
        .with_vault(vault)
        .with_blobs(blobs)
        .with_tts(tts, tts_cap)
        .with_admin(admin);
    let remote_cfg = ferrocast_relay::remote::RemoteConfig::from_env();
    match (&remote_cfg.origin, remote_cfg.dashboard_dir.is_some()) {
        (Some(o), true) => tracing::info!(origin = %o, "the dashboard from anywhere is on"),
        (Some(o), false) => {
            tracing::warn!(origin = %o, "RELAY_DASHBOARD_DIR not set: phones can pair, but /app/ has no dashboard")
        }
        (None, _) => tracing::warn!("RELAY_REMOTE_URL not set: the dashboard from anywhere is off"),
    }
    let state = state.with_remote(remote_cfg);
    let discord_cfg = match ferrocast_relay::discord::DiscordConfig::from_env() {
        Ok(Some(c)) => {
            tracing::info!(app_id = %c.app_id, "the Ferrocast bot for Discord is on");
            Some(c)
        }
        Ok(None) => {
            tracing::warn!(
                "DISCORD_APP_ID / DISCORD_BOT_TOKEN / DISCORD_PUBLIC_KEY / DISCORD_CLIENT_SECRET not set: the Discord bot is off"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "the Discord settings are incomplete: the Discord bot is off");
            None
        }
    };
    let state = state.with_discord(ferrocast_relay::discord::Runtime::from_config(discord_cfg));
    let twitch_cloud = match ferrocast_relay::bot::TwitchConfig::from_env() {
        Ok(Some(c)) => {
            tracing::info!(client_id = %c.client_id, "the cloud bot for Twitch is on");
            Some(c)
        }
        Ok(None) => {
            tracing::warn!(
                "TWITCH_CLOUD_CLIENT_ID / TWITCH_CLOUD_CLIENT_SECRET / TWITCH_CLOUD_EVENTSUB_SECRET not set: the Twitch cloud bot is off"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "the Twitch cloud bot settings are incomplete: it's off");
            None
        }
    };
    let youtube_cloud = match ferrocast_relay::bot::YoutubeConfig::from_env() {
        Ok(Some(c)) => {
            tracing::info!(
                client_id = %c.client_id, units_per_sub = c.units_per_sub, units = c.units,
                "the cloud bot for YouTube (beta) is on"
            );
            Some(c)
        }
        Ok(None) => {
            tracing::warn!(
                "YOUTUBE_CLOUD_CLIENT_ID / YOUTUBE_CLOUD_CLIENT_SECRET not set: the YouTube cloud bot is off"
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "the YouTube cloud bot settings are wrong: it's off");
            None
        }
    };
    let youtube_api = youtube_cloud
        .as_ref()
        .map(|c| Arc::new(ferrocast_relay::bot::HttpYoutube::new(c)) as Arc<dyn ferrocast_relay::bot::YoutubeCloud>);
    let bot_rt =
        ferrocast_relay::bot::Runtime::from_config(twitch_cloud, &state.cfg).with_youtube(youtube_cloud, youtube_api);
    let state = state.with_bot(bot_rt);
    let mailer: Option<Arc<dyn ferrocast_relay::mail::Mailer>> = match ferrocast_relay::mail::BrevoMailer::from_env() {
        Some(b) => {
            tracing::info!("the weekly email is on (Brevo)");
            Some(Arc::new(b))
        }
        None => {
            tracing::warn!("BREVO_API_KEY not set: the weekly email is off");
            None
        }
    };
    let state = state.with_mail(mailer, ferrocast_relay::mail::MailConfig::from_env());
    let tip_domains: Option<Arc<dyn ferrocast_relay::tips::domains::CustomHostnames>> =
        match ferrocast_relay::tips::domains::CloudflareHostnames::from_env() {
            Some(cf) => {
                tracing::info!("custom tip addresses are on (Cloudflare for SaaS)");
                Some(Arc::new(cf))
            }
            None => {
                tracing::warn!("CF_SAAS_TOKEN / CF_ZONE_ID not set: custom tip addresses are off");
                None
            }
        };
    // The TXT proof's values are keyed with RELAY_VAULT_KEY.
    let proof_secret = std::env::var("RELAY_VAULT_KEY")
        .ok()
        .and_then(|k| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, k.trim()).ok())
        .filter(|k| k.len() >= 16);
    if tip_domains.is_some() && proof_secret.is_none() {
        tracing::warn!("RELAY_VAULT_KEY not set: custom tip addresses are off (it keys their TXT proof)");
    }
    let state =
        state.with_tip_domains(tip_domains, None, proof_secret.as_deref(), ferrocast_relay::tips::domains::MAX_DOMAINS);
    ferrocast_relay::tips::domains::warm_up(&state);
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
    let remote = state.clone();
    tokio::spawn(async move {
        // Dashboard from anywhere: expired devices and codes, audit lines past 30 days.
        loop {
            ferrocast_relay::remote::sweep(&remote).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let discord = state.clone();
    tokio::spawn(async move {
        // Discord perks: ended roles taken back; servers of lapsed subscriptions after 30 days.
        loop {
            ferrocast_relay::discord::sweep(&discord).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let mail = state.clone();
    tokio::spawn(async move {
        // The weekly email: reports due this hour, then lapsed and unconfirmed addresses.
        loop {
            let now = ferrocast_relay::now_ms();
            ferrocast_relay::mail::run_weekly(&mail, now).await;
            ferrocast_relay::mail::sweep(&mail, now).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    let bot_timers = state.clone();
    // The cloud bot: timers while it's on duty, and the hourly sign-in check and lapse sweep.
    tokio::spawn(ferrocast_relay::bot::run(bot_timers));
    let bot_sweep = state.clone();
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ferrocast-relay: can't listen on {bind}: {e}");
            std::process::exit(1);
        }
    };
    // After the bind: the first sweep may point Twitch's shard here, and Twitch checks the
    // callback at once, so the relay must already take connections.
    tokio::spawn(async move {
        loop {
            ferrocast_relay::bot::sweep(&bot_sweep).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    tracing::info!(%bind, "relay listening");
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("ferrocast-relay: stopped: {e}");
        std::process::exit(1);
    }
}
