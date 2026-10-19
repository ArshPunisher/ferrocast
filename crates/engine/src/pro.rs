//! Ferrocast Pro as the app sees it (ARCHITECTURE.md §6). The relay owns billing; the
//! app asks it for this install's status and keeps the last answer, so Pro keeps working
//! offline: a status the relay confirmed stays valid for [`OFFLINE_GRACE_DAYS`] without
//! another check (never past the end of the paid period). The free app never needs any
//! of this (R-01, R-05): no Pro answer just means no Pro extras.

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Engine, EngineError, now};
use ferrocast_store::repo;

/// How long a status the relay confirmed is trusted without reaching it again.
pub const OFFLINE_GRACE_DAYS: i64 = 7;
const OFFLINE_GRACE_MS: i64 = OFFLINE_GRACE_DAYS * 86_400_000;
/// The relay is asked again at most this often unless the streamer presses Refresh.
const FRESH_MS: i64 = 10 * 60_000;
const CACHE_KEY: &str = "pro_status";

/// Shown in Settings › Pro until Pro is on sale.
pub const LAUNCH_NOTE: &str =
    "Ferrocast Pro launches after version 1. Everything Ferrocast does today is free, and stays free.";

/// Which Pro features the relay has: built and configured on that server. Pro panels show
/// only for these. An older relay without the field means none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProFeatures {
    pub cloud_bot: bool,
    pub remote: bool,
    pub tts: bool,
    pub discord: bool,
    pub backups: bool,
    pub tip_brand: bool,
    pub history: bool,
    pub weekly_email: bool,
}

/// One of the subscription's usage counters (API.md §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProQuota {
    pub used: i64,
    pub limit: i64,
    /// `day` or `month` (UTC calendar periods).
    pub period: String,
    /// When it starts again (ms).
    pub reset_at: i64,
}

/// The relay's answer (`GET /v1/pro/status`), as last received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProStatus {
    pub pro: bool,
    /// `none`, or Razorpay's subscription status (`active`, `pending`, `halted`, …).
    pub status: String,
    pub plan: Option<String>,
    pub subscription_id: Option<String>,
    pub current_end: Option<i64>,
    pub access_until: Option<i64>,
    #[serde(default)]
    pub cancel_at_period_end: bool,
    #[serde(default)]
    pub refunded: bool,
    #[serde(default)]
    pub installs: i64,
    #[serde(default)]
    pub sales_open: bool,
    #[serde(default)]
    pub prices: Option<Value>,
    /// When the relay said this, by this computer's clock (ms).
    #[serde(default)]
    pub checked_at: i64,
    #[serde(default)]
    pub features: ProFeatures,
    /// By counter name (`tts_chars`, …); empty without a subscription.
    #[serde(default)]
    pub quotas: std::collections::BTreeMap<String, ProQuota>,
}

impl ProStatus {
    fn none() -> ProStatus {
        ProStatus {
            pro: false,
            status: "none".into(),
            plan: None,
            subscription_id: None,
            current_end: None,
            access_until: None,
            cancel_at_period_end: false,
            refunded: false,
            installs: 0,
            sales_open: false,
            prices: None,
            checked_at: 0,
            features: ProFeatures::default(),
            quotas: Default::default(),
        }
    }
}

/// Whether a remembered status still grants Pro at `now`: the relay said so, within the
/// offline grace, and the paid period (with its grace) hasn't ended.
pub fn still_pro(s: &ProStatus, now: i64) -> bool {
    s.pro && now.saturating_sub(s.checked_at) <= OFFLINE_GRACE_MS && s.access_until.is_some_and(|u| now < u)
}

/// What the dashboard gets (`GET /api/v1/pro`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProView {
    #[serde(flatten)]
    pub status: ProStatus,
    /// Pro right now, offline grace included (use this, not `status.pro`).
    pub active: bool,
    /// This is a remembered answer: the relay couldn't be reached just now.
    pub stale: bool,
    pub offline_grace_days: i64,
    pub launch_note: &'static str,
    pub warning: Option<String>,
}

fn view(status: ProStatus, stale: bool, warning: Option<String>) -> ProView {
    let active = still_pro(&status, now().0);
    ProView { status, active, stale, offline_grace_days: OFFLINE_GRACE_DAYS, launch_note: LAUNCH_NOTE, warning }
}

pub(crate) fn unreachable(e: impl std::fmt::Display) -> EngineError {
    EngineError::Refused {
        status: 502,
        code: "relay_unreachable",
        message: format!("Couldn't reach the Ferrocast relay: {e}"),
        hint: "Check your internet connection and try again. Everything else in Ferrocast keeps working.".into(),
        retry_after: None,
    }
}

/// The relay's error codes the dashboard can act on, kept as they are.
fn code(c: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "pro_required",
        "vault_unavailable",
        "storage_unavailable",
        "too_large",
        "unsupported_type",
        "pro_not_on_sale",
        "already_pro",
        "already_cancelled",
        "not_cancellable",
        "no_subscription",
        "invalid_plan",
        "invalid_email",
        "invalid_subscription",
        "not_found",
        "rate_limited",
        "razorpay_unavailable",
        "billing_unavailable",
        "billing_misconfigured",
        "bad_json",
        // Stream history (API.md §9.1).
        "invalid",
        "invalid_range",
        "invalid_platform",
        "invalid_stream",
        "invalid_sample",
        "stream_not_found",
        "ended_stream_locked",
        "too_many_samples",
        "history_rate",
        // Tip page branding and goal (API.md §9.2).
        "image_unreadable",
        "tip_image_quota",
        "no_tip_page",
        // Cloud backups (API.md §9.3).
        "invalid_backup",
        "invalid_part",
        "parts_missing",
        "hash_mismatch",
        "upload_in_progress",
        "already_complete",
        "backup_quota",
        "backup_upload_quota",
        // Ferrocast voices (API.md §9.4).
        "tts_unavailable",
        "tts_quota",
        "tts_rate",
        "voice_not_found",
        "too_long",
        "empty_text",
        "storage",
        // Dashboard from anywhere (API.md §9.5).
        "remote_unavailable",
        "too_many_devices",
        "not_allowed_for_role",
        "pc_offline",
        "pc_timeout",
        "remote_rate",
        // The Ferrocast bot for Discord (API.md §9.6).
        "missing_permission",
        "guild_not_found",
        "not_connected",
        "already_connected",
        "channel_not_found",
        "no_channel",
        "too_many_rules",
        "invalid_rule",
        "role_not_found",
        "rule_not_found",
        "invalid_event",
        "too_many_events",
        "discord_rate",
        "discord_unavailable",
        // The weekly email (API.md §9.8).
        "mail_unavailable",
        "mail_failed",
        "mail_rate",
        "mail_busy",
        "already_on",
        // The 24/7 cloud bot (API.md §9.7).
        "stale_version",
        "too_many_commands",
        "too_many_timers",
        "invalid_config",
        "platform_not_found",
        "platform_unavailable",
        "bot_account_first",
        "kick_not_linked",
        "no_platform",
        "bot_rate",
        "twitch_not_linked",
        "wrong_twitch_app",
        "twitch_unreachable",
        "twitch_rate",
        "connect_again",
        // The YouTube cloud bot (M12, API.md §9.7).
        "youtube_not_linked",
        "wrong_youtube_app",
        "youtube_unreachable",
        "youtube_rate",
        "youtube_quota",
        "youtube_no_channel",
        // The tip page's own address (API.md §9.2).
        "domains_unavailable",
        "invalid_hostname",
        "apex_not_supported",
        "domain_limit",
        "domain_taken",
        "domains_full",
        "cloudflare_unavailable",
        "tip_domain_quota",
    ];
    KNOWN.iter().copied().find(|k| *k == c).unwrap_or("relay_error")
}

/// Turns a relay error answer into the same status, code, message and hint.
pub(crate) async fn relay_error(res: reqwest::Response) -> EngineError {
    let status = res.status();
    // The relay's own wait (a quota's reset), else a minute.
    let wait = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let v: Value = res.json().await.unwrap_or(Value::Null);
    relay_error_parts(status.as_u16(), wait, &v)
}

/// [`relay_error`] from an answer already read: its status, `Retry-After` and body.
pub(crate) fn relay_error_parts(status: u16, wait: Option<u64>, v: &Value) -> EngineError {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = |k: &str| v.pointer(&format!("/error/{k}")).and_then(Value::as_str).unwrap_or("").to_string();
    let message = text("message");
    if status == StatusCode::UNAUTHORIZED {
        // The relay forgot this install (the relay link registers it again by itself).
        return EngineError::Refused {
            status: 503,
            code: "relay_unrecognised",
            message: "The Ferrocast relay didn't recognise this install.".into(),
            hint: "Try again: Ferrocast registers with the relay again. If you had Pro, use Restore in Settings › Pro."
                .into(),
            retry_after: None,
        };
    }
    let status = match status.as_u16() {
        s @ (400 | 402 | 403 | 404 | 409 | 413 | 415 | 422 | 429 | 502 | 503) => s,
        _ => 502,
    };
    EngineError::Refused {
        status,
        code: code(&text("code")),
        message: if message.is_empty() { "The Ferrocast relay refused that.".into() } else { message },
        hint: text("hint"),
        retry_after: (status == 429).then(|| wait.unwrap_or(60)),
    }
}

impl Engine {
    async fn pro_cached(&self) -> Option<ProStatus> {
        self.db(|s| repo::settings::get::<ProStatus>(s, CACHE_KEY)).await.ok().flatten()
    }

    async fn pro_remember(&self, mut s: ProStatus) -> ProStatus {
        s.checked_at = now().0;
        let saved = s.clone();
        if let Err(e) = self.db(move |st| repo::settings::set(st, CACHE_KEY, &saved)).await {
            tracing::warn!(error = %e, "couldn't remember the Pro status");
        }
        s
    }

    /// Calls the relay with this install's identity.
    pub(crate) async fn pro_call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, EngineError> {
        self.pro_call_status(method, path, body).await.map(|(_, v)| v)
    }

    /// [`Engine::pro_call`], with the relay's success status (200 or 201) for a caller that
    /// passes it on.
    pub(crate) async fn pro_call_status(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), EngineError> {
        self.pro_call_passing(method, path, body, |_, _| false).await
    }

    /// [`Engine::pro_call_status`], except that an error answer `pass` accepts comes back as
    /// `Ok((status, body))` for the caller to pass on whole (a 409 that carries the DNS record
    /// to add, say). Its code is still checked against the known list.
    pub(crate) async fn pro_call_passing(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        pass: fn(u16, &Value) -> bool,
    ) -> Result<(u16, Value), EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let mut req = self.relay_http().request(method, format!("{url}{path}")).bearer_auth(auth);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| unreachable(e.without_url()))?;
        if res.status() == StatusCode::UNAUTHORIZED {
            // The relay doesn't know this install any more: forget it, so the next call
            // registers a fresh one (the relay link does the same).
            if let Err(e) = self.inner.secrets.delete(crate::relay::INSTALL) {
                tracing::warn!(error = %e, "couldn't forget the relay install");
            }
        }
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let wait = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if pass(status, &v) && code(v.pointer("/error/code").and_then(Value::as_str).unwrap_or("")) != "relay_error"
            {
                return Ok((status, v));
            }
            return Err(relay_error_parts(status, wait, &v));
        }
        let status = res.status().as_u16();
        let v = res.json().await.map_err(|_| unreachable("it sent an answer Ferrocast can't read"))?;
        Ok((status, v))
    }

    /// Pro for this install. Uses the remembered answer when it's recent (unless
    /// `refresh`), asks the relay otherwise, and falls back to the remembered answer
    /// (marked stale) when the relay can't be reached. Never an error: no answer at all
    /// is "no Pro".
    pub async fn pro_status(&self, refresh: bool) -> ProView {
        let cached = self.pro_cached().await;
        if !refresh
            && let Some(c) = &cached
            && now().0.saturating_sub(c.checked_at) < FRESH_MS
        {
            return view(c.clone(), false, None);
        }
        if self.relay_url().is_err() {
            return view(cached.unwrap_or_else(ProStatus::none), false, Some("This build has no relay set up.".into()));
        }
        let path = if refresh { "/v1/pro/status?refresh=1" } else { "/v1/pro/status" };
        match self.pro_call(reqwest::Method::GET, path, None).await {
            Ok(v) => match serde_json::from_value::<ProStatus>(v.clone()) {
                Ok(s) => {
                    let warning = v["warning"].as_str().map(String::from);
                    view(self.pro_remember(s).await, false, warning)
                }
                Err(_) => self.pro_offline(cached, "the relay sent an answer Ferrocast can't read"),
            },
            Err(e) => self.pro_offline(cached, &e.to_string()),
        }
    }

    fn pro_offline(&self, cached: Option<ProStatus>, why: &str) -> ProView {
        tracing::debug!(why, "Pro status from memory");
        let warning = match &cached {
            Some(_) => "Couldn't reach the Ferrocast relay, so this is the last status it confirmed.",
            None => "Couldn't reach the Ferrocast relay to check Pro.",
        };
        view(cached.unwrap_or_else(ProStatus::none), true, Some(warning.into()))
    }

    /// Pro right now, from memory only (no network): for gating Pro extras.
    pub async fn is_pro(&self) -> bool {
        self.pro_cached().await.is_some_and(|s| still_pro(&s, now().0))
    }

    /// Pro right now and this relay has the feature, from memory only (no network).
    pub(crate) async fn pro_has(&self, feature: fn(&ProFeatures) -> bool) -> bool {
        self.pro_cached().await.is_some_and(|s| still_pro(&s, now().0) && feature(&s.features))
    }

    /// A Pro file from the relay (a CSV export): its content type and bytes.
    pub(crate) async fn pro_download(&self, path: &str) -> Result<(String, Vec<u8>), EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let res = self
            .relay_http()
            .get(format!("{url}{path}"))
            .bearer_auth(auth)
            .send()
            .await
            .map_err(|e| unreachable(e.without_url()))?;
        if !res.status().is_success() {
            return Err(relay_error(res).await);
        }
        let ct = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = res.bytes().await.map_err(|e| unreachable(e.without_url()))?;
        Ok((ct, bytes.to_vec()))
    }

    /// A Pro call whose body isn't JSON (an image) or whose answer may be empty (204):
    /// `body` is `(content type, bytes)`; `None` back for an empty answer.
    pub(crate) async fn pro_raw(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<Option<Value>, EngineError> {
        let url = self.relay_url()?;
        let auth = self.relay_auth().await.map_err(unreachable)?;
        let mut req = self.relay_http().request(method, format!("{url}{path}")).bearer_auth(auth);
        if let Some((ct, bytes)) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, ct).body(bytes);
        }
        let res = req.send().await.map_err(|e| unreachable(e.without_url()))?;
        if !res.status().is_success() {
            return Err(relay_error(res).await);
        }
        let bytes = res.bytes().await.map_err(|e| unreachable(e.without_url()))?;
        if bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|_| unreachable("it sent an answer Ferrocast can't read"))
    }

    /// Starts checkout: the link to Razorpay's page, to open in the browser.
    pub async fn pro_checkout(&self, plan: &str, email: &str) -> Result<Value, EngineError> {
        let v = self
            .pro_call(reqwest::Method::POST, "/v1/pro/checkout", Some(json!({ "plan": plan, "email": email })))
            .await?;
        let url = v["checkout_url"].as_str().unwrap_or("");
        if !url.starts_with("https://") {
            return Err(unreachable("it sent no checkout link"));
        }
        Ok(json!({ "checkout_url": url, "subscription_id": v["subscription_id"], "plan": v["plan"],
                   "amount": v["amount"], "currency": v["currency"] }))
    }

    /// A short-lived link to the relay's manage page (status, stop renewing).
    pub async fn pro_manage_link(&self) -> Result<Value, EngineError> {
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/manage-link", None).await?;
        Ok(json!({ "url": v["url"], "expires_at": v["expires_at"] }))
    }

    /// Stops renewing; Pro lasts until the end of the paid period.
    pub async fn pro_cancel(&self) -> Result<ProView, EngineError> {
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/cancel", None).await?;
        let s = serde_json::from_value(v).map_err(|_| unreachable("it sent an answer Ferrocast can't read"))?;
        Ok(view(self.pro_remember(s).await, false, None))
    }

    /// Moves Pro onto this install with the subscription id and the checkout email.
    pub async fn pro_restore(&self, subscription_id: &str, email: &str) -> Result<ProView, EngineError> {
        let body = json!({ "subscription_id": subscription_id.trim(), "email": email.trim() });
        let v = self.pro_call(reqwest::Method::POST, "/v1/pro/restore", Some(body)).await?;
        let s = serde_json::from_value(v).map_err(|_| unreachable("it sent an answer Ferrocast can't read"))?;
        Ok(view(self.pro_remember(s).await, false, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(pro: bool, checked_at: i64, access_until: Option<i64>) -> ProStatus {
        ProStatus { pro, checked_at, access_until, status: "active".into(), ..ProStatus::none() }
    }

    #[test]
    fn a_remembered_status_holds_offline_for_the_grace_only() {
        let t = 1_800_000_000_000;
        let far = Some(t + 365 * 86_400_000);
        assert!(still_pro(&status(true, t, far), t));
        assert!(still_pro(&status(true, t, far), t + OFFLINE_GRACE_MS), "the last day of the grace");
        assert!(!still_pro(&status(true, t, far), t + OFFLINE_GRACE_MS + 1), "too long without the relay");
        assert!(!still_pro(&status(true, t, Some(t + 1000)), t + 1000), "the paid period ended, even offline");
        assert!(!still_pro(&status(false, t, far), t), "not Pro stays not Pro");
        assert!(!still_pro(&status(true, t, None), t), "no period, no Pro");
        assert!(still_pro(&status(true, t, far), t - 5_000), "a clock that went back a little is fine");
    }

    #[test]
    fn relay_codes_pass_through_and_unknown_ones_are_generic() {
        assert_eq!(code("pro_not_on_sale"), "pro_not_on_sale");
        assert_eq!(code("rate_limited"), "rate_limited");
        assert_eq!(code("<script>"), "relay_error");
        assert_eq!(code(""), "relay_error");
    }

    #[test]
    fn an_old_relay_answer_without_new_fields_still_reads() {
        let v = json!({ "pro": true, "status": "active", "plan": "monthly", "subscription_id": "sub_1",
                        "current_end": 5, "access_until": 6 });
        let s: ProStatus = serde_json::from_value(v).unwrap();
        assert!(s.pro && !s.sales_open && s.checked_at == 0);
        assert_eq!(s.features, ProFeatures::default(), "no features field: none");
        assert!(s.quotas.is_empty());
    }

    #[test]
    fn features_and_quotas_pass_through() {
        let v = json!({ "pro": true, "status": "active", "plan": "monthly", "subscription_id": "sub_1",
                        "current_end": 5, "access_until": 6,
                        "features": { "tts": true, "history": true, "something_new": true },
                        "quotas": { "tts_chars": { "used": 120, "limit": 20000, "period": "month", "reset_at": 9 } } });
        let s: ProStatus = serde_json::from_value(v).unwrap();
        assert!(s.features.tts && s.features.history && !s.features.cloud_bot, "unknown features are ignored");
        assert_eq!(s.quotas["tts_chars"].used, 120);
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["features"]["backups"], false);
        assert_eq!(out["quotas"]["tts_chars"]["limit"], 20000);
    }

    #[test]
    fn the_pro_gate_and_size_errors_keep_their_status() {
        assert_eq!(code("pro_required"), "pro_required");
        assert_eq!(code("vault_unavailable"), "vault_unavailable");
    }
}
