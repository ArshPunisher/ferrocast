//! `GET /admin/v1/stats`: who may call it, what it counts, and that nothing personal leaves.

use rusqlite::params;
use serde_json::Value;

use super::*;
use crate::{Config, router};

/// 64 hex characters, like `control-center-key.sh` makes (built in pieces so it never looks
/// like a real secret to a scanner).
fn token() -> String {
    ["0123456789abcdef"; 4].concat()
}

struct Relay {
    base: String,
    st: AppState,
    dir: tempfile::TempDir,
}

async fn relay(token: Option<&str>) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let marker = dir.path().join(BACKUP_MARKER);
    let st = AppState::new(cfg, db).with_admin(AdminConfig::new(token, Some(marker)));
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    Relay { base, st, dir }
}

async fn get(r: &Relay, headers: &[(&str, &str)]) -> (u16, HeaderMap, Value) {
    let mut req = reqwest::Client::new().get(format!("{}/admin/v1/stats", r.base));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    (status, headers, res.json().await.unwrap_or(Value::Null))
}

fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}

#[tokio::test]
async fn off_without_a_token_with_a_hint() {
    let r = relay(None).await;
    let (status, _, body) = get(&r, &[("authorization", &bearer(&token()))]).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "admin_off");
    assert!(body["error"]["hint"].as_str().unwrap().contains("control-center-key.sh"));

    // A token that's too short is refused as if unset, and says why.
    let r = relay(Some("short-token")).await;
    let (status, _, body) = get(&r, &[("authorization", "Bearer short-token")]).await;
    assert_eq!(status, 503);
    assert!(body["error"]["message"].as_str().unwrap().contains("shorter than 32"));
    assert!(body["error"]["hint"].as_str().unwrap().contains("--rotate"));
}

#[tokio::test]
async fn needs_the_right_bearer_token() {
    let r = relay(Some(&token())).await;
    let wrong_len = bearer("x");
    let almost = bearer(&format!("{}0", &token()[..63]));
    let basic = format!("Basic {}", token());
    for (case, headers) in [
        ("no header", vec![]),
        ("empty bearer", vec![("authorization", "Bearer ")]),
        ("other scheme", vec![("authorization", basic.as_str())]),
        ("wrong length", vec![("authorization", wrong_len.as_str())]),
        ("one character off", vec![("authorization", almost.as_str())]),
    ] {
        let (status, h, body) = get(&r, &headers).await;
        assert_eq!(status, 401, "{case}");
        assert_eq!(body["error"]["code"], "unauthorized", "{case}");
        assert!(body["error"]["hint"].as_str().unwrap().contains("RELAY_ADMIN_TOKEN"), "{case}");
        assert_eq!(h.get("www-authenticate").unwrap(), "Bearer", "{case}");
    }
    let (status, h, body) = get(&r, &[("authorization", &bearer(&token()))]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(h.get("cache-control").unwrap(), "no-store");
    // Surrounding spaces don't matter (an env file with a trailing space).
    let spaced = format!("Bearer  {}  ", token());
    assert_eq!(get(&r, &[("authorization", &spaced)]).await.0, 200);
}

#[tokio::test]
async fn nothing_through_a_public_address() {
    let r = relay(Some(&token())).await;
    let auth = bearer(&token());
    for h in ["cf-connecting-ip", "cf-ray", "cdn-loop", "x-forwarded-for", "forwarded"] {
        let (status, _, body) = get(&r, &[("authorization", &auth), (h, "1.2.3.4")]).await;
        assert_eq!(status, 403, "{h}");
        assert_eq!(body["error"]["code"], "internal_only", "{h}");
    }
    // Refused before the allowance is touched: the internet can't use it up.
    assert_eq!(r.st.admin.minute.lock().unwrap().1, 0);
}

#[test]
fn proxied_requests_are_spotted() {
    let mut h = HeaderMap::new();
    assert!(!proxied(&h));
    h.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
    assert!(proxied(&h));
}

#[test]
fn the_allowance_is_per_minute() {
    let rt = Runtime::new(AdminConfig::new(Some(&token()), None));
    let t = 1_760_000_000_000 - 1_760_000_000_000 % 60_000 + 15_000; // 15 s into a minute
    for _ in 0..MAX_PER_MINUTE {
        rt.take(t).unwrap();
    }
    assert_eq!(rt.take(t), Err(45));
    assert_eq!(rt.take(t + 44_500), Err(1), "never 0 seconds");
    assert!(rt.take(t + 45_000).is_ok(), "a new minute starts afresh");
}

#[tokio::test]
async fn too_many_requests_get_429_with_retry_after() {
    let r = relay(Some(&token())).await;
    let auth = bearer(&token());
    // Two minutes' worth at most: a minute boundary can reset the count once.
    let mut limited = None;
    for _ in 0..(2 * MAX_PER_MINUTE + 5) {
        let (status, h, body) = get(&r, &[("authorization", &auth)]).await;
        if status == 429 {
            limited = Some((h, body));
            break;
        }
        assert_eq!(status, 200);
    }
    let (h, body) = limited.expect("the allowance never ran out");
    assert_eq!(body["error"]["code"], "rate_limited");
    let wait: i64 = h.get("retry-after").unwrap().to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&wait));
}

#[test]
fn mrr_counts_a_twelfth_of_yearly() {
    assert_eq!(mrr_paise(0, 0), 0);
    assert_eq!(mrr_paise(1, 0), 19_900);
    assert_eq!(mrr_paise(0, 1), 16_658); // ₹1,999 / 12 = ₹166.58
    assert_eq!(mrr_paise(0, 12), 199_900);
    assert_eq!(mrr_paise(3, 2), 3 * 19_900 + 33_317);
}

#[test]
fn months_start_on_the_first_utc() {
    // 2026-10-20T12:00:00Z → 2026-10-01T00:00:00Z
    assert_eq!(month_start(1_792_497_600_000), 1_790_812_800_000);
    assert_eq!(month_start(1_790_812_800_000), 1_790_812_800_000);
    assert_eq!(month_start(1_790_812_799_999), 1_788_220_800_000);
}

#[test]
fn the_backup_marker_must_be_one_utc_time() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(BACKUP_MARKER);
    assert_eq!(read_marker(&p), None, "missing");
    for (text, want) in [
        ("2026-10-20T21:30:12Z\n", Some("2026-10-20T21:30:12Z")),
        ("2026-10-20T21:30:12Z", Some("2026-10-20T21:30:12Z")),
        ("2026-13-20T21:30:12Z", None),
        ("2026-10-20 21:30:12Z", None),
        ("<script>alert(1)</script>", None),
        ("", None),
    ] {
        std::fs::write(&p, text).unwrap();
        assert_eq!(read_marker(&p).as_deref(), want, "{text:?}");
    }
    std::fs::write(&p, "2026-10-20T21:30:12Z".repeat(1000)).unwrap();
    assert_eq!(read_marker(&p), None, "huge");
}

const H: i64 = 3_600_000;

/// A relay with a bit of everything, `now` at least two days into a UTC month.
fn seed(db: &Db) -> i64 {
    let real = now_ms();
    let now = if real - month_start(real) > 2 * DAY_MS { real } else { real + 3 * DAY_MS };
    let month = month_start(now);
    let c = db.c();
    for (id, created, seen) in [
        ("inst_old_active", now - 60 * DAY_MS, now - DAY_MS),
        ("inst_new_active", now - 2 * DAY_MS, now - H),
        ("inst_never_used", now - 3 * DAY_MS, now - 3 * DAY_MS),
        ("inst_month_ago", now - 90 * DAY_MS, now - 20 * DAY_MS),
    ] {
        c.execute(
            "INSERT INTO installs(id, secret_hash, created_at, last_seen) VALUES (?1, 'SECRETHASH', ?2, ?3)",
            params![id, created, seen],
        )
        .unwrap();
    }
    // (id, plan, status, current_end, cancel_at_end, first payment, install)
    for (id, plan, status, end, cancel, paid, install) in [
        ("sub_monthly", "monthly", "active", Some(now + 20 * DAY_MS), 0, Some(now - H), Some("inst_old_active")),
        ("sub_yearly", "yearly", "active", Some(now + 300 * DAY_MS), 1, Some(month - DAY_MS), Some("inst_new_active")),
        ("sub_gone", "monthly", "cancelled", Some(now - H), 1, Some(month - 20 * DAY_MS), Some("inst_month_ago")),
        ("sub_abandoned", "monthly", "created", None, 0, None, None),
        ("comp_owner", comp::PLAN, "active", Some(now + 10 * DAY_MS), 1, None, Some("inst_never_used")),
    ] {
        c.execute(
            "INSERT INTO pro_subs(id, plan, email_hash, status, current_end, cancel_at_end, created_at, synced_at)
             VALUES (?1, ?2, 'EMAILHASH', ?3, ?4, ?5, ?6, ?6)",
            params![id, plan, status, end, cancel, now - 40 * DAY_MS],
        )
        .unwrap();
        if let Some(at) = paid {
            c.execute(
                "INSERT INTO pro_payments(payment_id, sub_id, amount, at) VALUES (?1, ?2, 19900, ?3)",
                params![format!("pay_{id}"), id, at],
            )
            .unwrap();
        }
        if let Some(i) = install {
            c.execute(
                "INSERT INTO pro_installs(install_id, sub_id, linked_at) VALUES (?1, ?2, ?3)",
                params![i, id, now],
            )
            .unwrap();
        }
    }
    c.execute_batch(&format!(
        "INSERT INTO bot_channels(sub_id, platform, channel_login, connected_by, connected_at) VALUES
           ('sub_monthly', 'twitch', 'secret_login', 'inst_old_active', {now}),
           ('sub_monthly', 'kick', 'secret_login', 'inst_old_active', {now}),
           ('sub_gone', 'twitch', 'gone_login', 'inst_month_ago', {now});
         INSERT INTO bot_config(sub_id, version, json, updated_at, enabled) VALUES
           ('sub_monthly', 1, '{{}}', {now}, 1), ('sub_gone', 1, '{{}}', {now}, 1), ('sub_yearly', 1, '{{}}', {now}, 0);
         INSERT INTO discord_guilds(sub_id, guild_id, guild_name, link_code, connected_by, connected_at, updated_at) VALUES
           ('sub_monthly', '111', 'Secret Guild Name', 'CODE1', 'inst_old_active', {now}, {now}),
           ('sub_gone', '222', 'Gone Guild', 'CODE2', 'inst_month_ago', {now}, {now});
         INSERT INTO mail_addresses(sub_id, status, created_at, updated_at) VALUES
           ('sub_monthly', 'on', {now}, {now}), ('sub_yearly', 'paused', {now}, {now});
         INSERT INTO cloud_backups(id, sub_id, install_id, bytes, sha256, app_version, reason, part_size, state, created_at, updated_at) VALUES
           ('b1', 'sub_yearly', 'inst_new_active', 10, 'x', '1.1.0', 'manual', 10, 'complete', {now}, {now}),
           ('b2', 'sub_yearly', 'inst_new_active', 10, 'x', '1.1.0', 'manual', 10, 'complete', {now}, {now}),
           ('b3', 'comp_owner', 'inst_never_used', 10, 'x', '1.1.0', 'manual', 10, 'uploading', {now}, {now});
         INSERT INTO remote_devices(id, install_id, sub_id, role, name, token_hash, created_at, last_used_at, expires_at) VALUES
           ('d1', 'inst_old_active', 'sub_monthly', 'owner', 'Secret Phone', 'h', {now}, {now}, {later}),
           ('d2', 'inst_old_active', 'sub_monthly', 'owner', 'Old Phone', 'h', {now}, {now}, {earlier});
         INSERT INTO tip_pages(install_id, handle, config) VALUES ('inst_old_active', 'secret-handle', '{{}}');
         INSERT INTO tip_brand(install_id, updated_at) VALUES ('inst_old_active', {now});
         INSERT INTO tip_goals(id, install_id, title, target_minor, currency, starts_at, updated_at) VALUES
           ('g1', 'inst_old_active', 'Secret goal title', 500000, 'INR', {now}, {now});
         INSERT INTO tip_goal_payments(goal_id, payment_hash, amount_minor) VALUES ('g1', 'p1', 7777777), ('g1', 'p2', 7777777);
         INSERT INTO tts_usage(scope, month, chars, requests, updated_at) VALUES ('all', '{month_key}', 4321, 7, {now});",
        later = now + DAY_MS,
        earlier = now - DAY_MS,
        month_key = Period::Month.key(now),
    ))
    .unwrap();
    now
}

#[tokio::test]
async fn counts_add_up() {
    let r = relay(Some(&token())).await;
    let now = seed(&r.st.db);
    let v = collect(&r.st, now).unwrap();

    assert_eq!(v["relay"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(v["relay"]["uptime_seconds"].as_i64().unwrap() >= 0);
    assert_eq!(
        v["installs"],
        serde_json::json!({ "total": 4, "active_7d": 2, "active_30d": 3, "new_7d": 2, "new_30d": 2 })
    );
    let pro = &v["pro"];
    assert_eq!(pro["active"], 2, "{pro}");
    assert_eq!(pro["monthly"], 1);
    assert_eq!(pro["yearly"], 1);
    assert_eq!(pro["other"], 0);
    assert_eq!(pro["cancelling"], 1);
    assert_eq!(pro["mrr_paise"], 19_900 + 16_658);
    assert_eq!(pro["new_this_month"], 1, "only the first charge this month counts as new");
    assert_eq!(pro["ended_this_month"], 1);
    assert_eq!(pro["installs"], 2);
    assert_eq!(pro["comp"], serde_json::json!({ "active": 1, "installs": 1 }), "test grants apart");

    let u = &v["usage"];
    assert_eq!(u["tts"]["chars"], 4321);
    assert_eq!(u["tts"]["requests"], 7);
    assert_eq!(u["tts"]["month"], Period::Month.key(now).trim_start_matches('m'));
    // Lapsed subscriptions' leftovers aren't counted.
    assert_eq!(u["cloud_bot"], serde_json::json!({ "on": 1, "channels": 2, "twitch": 1, "kick": 1, "youtube": 0 }));
    assert_eq!(u["discord"]["guilds"], 1);
    assert_eq!(u["weekly_email"]["opted_in"], 1);
    assert_eq!(u["cloud_backups"]["subscriptions"], 1);
    assert_eq!(u["remote"]["devices"], 1);
    assert_eq!(u["tips"], serde_json::json!({ "pages": 1, "branded": 1, "goals": 1, "goal_tips": 2 }));
    assert_eq!(v["backup"]["last_ok_at"], Value::Null);

    std::fs::write(r.dir.path().join(BACKUP_MARKER), "2026-10-20T21:30:12Z\n").unwrap();
    let v = collect(&r.st, now).unwrap();
    assert_eq!(v["backup"]["last_ok_at"], "2026-10-20T21:30:12Z");
}

#[tokio::test]
async fn an_empty_relay_counts_zero() {
    let r = relay(Some(&token())).await;
    let (status, _, v) = get(&r, &[("authorization", &bearer(&token()))]).await;
    assert_eq!(status, 200);
    assert_eq!(v["installs"]["total"], 0);
    assert_eq!(v["pro"]["active"], 0);
    assert_eq!(v["pro"]["mrr_paise"], 0);
    assert_eq!(v["usage"]["tts"]["chars"], 0);
}

/// Every string in the answer, with its path; numbers equal to `amount`.
fn walk(v: &Value, path: &str, strings: &mut Vec<(String, String)>, amount: i64, hits: &mut Vec<String>) {
    match v {
        Value::String(s) => strings.push((path.to_string(), s.clone())),
        Value::Number(n) if n.as_i64() == Some(amount) => hits.push(path.to_string()),
        Value::Array(a) => {
            a.iter().enumerate().for_each(|(i, x)| walk(x, &format!("{path}[{i}]"), strings, amount, hits))
        }
        Value::Object(o) => o.iter().for_each(|(k, x)| walk(x, &format!("{path}.{k}"), strings, amount, hits)),
        _ => {}
    }
}

#[tokio::test]
async fn nothing_personal_leaves() {
    let r = relay(Some(&token())).await;
    seed(&r.st.db);
    std::fs::write(r.dir.path().join(BACKUP_MARKER), "2026-10-20T21:30:12Z").unwrap();
    let (status, _, v) = get(&r, &[("authorization", &bearer(&token()))]).await;
    assert_eq!(status, 200);
    let text = v.to_string();
    for secret in [
        "inst_",
        "sub_monthly",
        "sub_yearly",
        "comp_owner",
        "EMAILHASH",
        "SECRETHASH",
        "pay_",
        "secret_login",
        "Secret Guild",
        "secret-handle",
        "Secret Phone",
        "Secret goal",
        "CODE1",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(!text.contains(&token()), "the token itself");
    // The only strings are these, and no number is a tip amount.
    let (mut strings, mut hits) = (Vec::new(), Vec::new());
    walk(&v, "", &mut strings, 7_777_777, &mut hits);
    let mut paths: Vec<&str> = strings.iter().map(|(p, _)| p.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [".backup.last_ok_at", ".pro.currency", ".relay.version", ".usage.tts.month"],
        "unexpected text fields: {strings:?}"
    );
    assert!(hits.is_empty(), "a tip amount leaked at {hits:?}");
}
