//! Discord go-live posts: settings (the webhook stays in the secret store) and a test post;
//! and the Ferrocast bot (Pro), passed through to the relay.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::extract::Body;

pub async fn get(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(app.engine.discord().await?)))
}

/// `{ webhook_url?: string | null, enabled?, template?, mention_everyone? }`: only what's
/// sent changes. The response never contains the webhook URL.
pub async fn save(State(app): State<AppState>, Body(patch): Body<Value>) -> ApiResult<Json<Value>> {
    let Some(patch) = patch.as_object() else {
        return Err(ApiError::invalid("The Discord settings must be an object."));
    };
    Ok(Json(json!(app.engine.save_discord(patch).await?)))
}

pub async fn test(State(app): State<AppState>) -> ApiResult<StatusCode> {
    match app.engine.test_discord().await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(f) => {
            let mut e = ApiError::from(f.error);
            if let Some(s) = f.retry_after {
                e.retry_after = Some(s);
            }
            Err(e)
        }
    }
}

// ── The Ferrocast bot (Pro, API.md §9.6): the relay's routes passed through ──────────────

/// `GET /pro/discord` → the bot's view (server, channels, roles, go-live, rules, links).
pub async fn bot(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.discord_bot().await?))
}

/// `POST /pro/discord/connect` → 201 `{invite_url, expires_at}` (open it in the browser).
pub async fn bot_connect(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(app.engine.discord_bot_connect().await?)))
}

fn object(v: Value) -> Result<Value, ApiError> {
    if v.is_object() { Ok(v) } else { Err(ApiError::invalid("The request must be a JSON object.")) }
}

/// `PUT /pro/discord/channel {channel_id}`.
pub async fn bot_channel(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.discord_bot_put("/channel", object(b)?).await?))
}

/// `PUT /pro/discord/go-live {enabled, message?}`.
pub async fn bot_go_live(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.discord_bot_put("/go-live", object(b)?).await?))
}

/// `PUT /pro/discord/rules {rule}` → 201 with the new view.
pub async fn bot_add_rule(State(app): State<AppState>, Body(b): Body<Value>) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(app.engine.discord_bot_put("/rules", object(b)?).await?)))
}

/// Rule ids are the relay's `dr_` + hex: anything else never reaches it.
fn rule_path(id: &str) -> Result<String, ApiError> {
    let ok = id.len() <= 40 && id.starts_with("dr_") && id[3..].bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(format!("/rules/{id}"))
    } else {
        Err(ApiError::new(StatusCode::NOT_FOUND, "rule_not_found", "That perk rule doesn't exist any more."))
    }
}

/// `PUT /pro/discord/rules/{id}`.
pub async fn bot_update_rule(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Body(b): Body<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(app.engine.discord_bot_put(&rule_path(&id)?, object(b)?).await?))
}

/// `DELETE /pro/discord/rules/{id}` → 204.
pub async fn bot_delete_rule(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.engine.discord_bot_delete(&rule_path(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /pro/discord` → 204: the bot leaves the server.
pub async fn bot_disconnect(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.engine.discord_bot_delete("").await?;
    Ok(StatusCode::NO_CONTENT)
}

/// End to end: dashboard API → engine → a real relay with a stand-in for Discord.
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrocast_engine::{Engine, PlatformApps};
    use ferrocast_relay::billing::store::Synced;
    use ferrocast_relay::discord::{
        self as rd, Channel, Connection, DiscordApi, DiscordError, Fut, Guild, Role, Token,
    };
    use ferrocast_secrets::Secrets;
    use reqwest::Method;
    use serde_json::{Value, json};

    use crate::{ServeOptions, serve};

    const GUILD: &str = "900000000000000001";

    /// Just enough Discord: one server, one channel, a role above the bot's.
    #[derive(Debug)]
    struct Stand;

    impl DiscordApi for Stand {
        fn exchange_code<'a>(&'a self, _code: &'a str, _redirect: &'a str) -> Fut<'a, Token> {
            Box::pin(async {
                Ok(Token {
                    access_token: "t".into(),
                    guild: Some(Guild { id: GUILD.into(), name: "Lounge".into(), icon: None }),
                })
            })
        }
        fn revoke<'a>(&'a self, _t: &'a str) -> Fut<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn me<'a>(&'a self, _t: &'a str) -> Fut<'a, String> {
            Box::pin(async { Err(DiscordError::Unauthorized) })
        }
        fn connections<'a>(&'a self, _t: &'a str) -> Fut<'a, Vec<Connection>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn guild<'a>(&'a self, g: &'a str) -> Fut<'a, Guild> {
            Box::pin(async move { Ok(Guild { id: g.into(), name: "Lounge".into(), icon: None }) })
        }
        fn channels<'a>(&'a self, _g: &'a str) -> Fut<'a, Vec<Channel>> {
            Box::pin(async {
                Ok(vec![Channel { id: "900000000000000010".into(), name: "live".into(), kind: 0, position: 0 }])
            })
        }
        fn roles<'a>(&'a self, _g: &'a str) -> Fut<'a, Vec<Role>> {
            Box::pin(async {
                let r =
                    |id: &str, position| Role { id: id.into(), name: id.into(), color: 0, position, managed: false };
                Ok(vec![r("900000000000000020", 1), r("900000000000000021", 2), r("900000000000000022", 9)])
            })
        }
        fn bot_roles<'a>(&'a self, _g: &'a str) -> Fut<'a, Vec<String>> {
            Box::pin(async { Ok(vec!["900000000000000021".into()]) })
        }
        fn add_role<'a>(&'a self, _g: &'a str, _u: &'a str, _r: &'a str) -> Fut<'a, ()> {
            Box::pin(async { Err(DiscordError::MissingPermission) })
        }
        fn remove_role<'a>(&'a self, _g: &'a str, _u: &'a str, _r: &'a str) -> Fut<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn post<'a>(&'a self, _c: &'a str, _b: Value) -> Fut<'a, String> {
            Box::pin(async { Ok("700000000000000001".into()) })
        }
        fn edit<'a>(&'a self, _c: &'a str, _m: &'a str, _b: Value) -> Fut<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn leave<'a>(&'a self, _g: &'a str) -> Fut<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn the_bot_section_comes_through_from_the_relay_with_its_errors() {
        let rdir = tempfile::tempdir().unwrap();
        let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = format!("http://{}", rl.local_addr().unwrap());
        let cfg = ferrocast_relay::Config {
            public_url: relay.clone(),
            tip_url: None,
            kick_client_id: None,
            kick_client_secret: None,
            kick_id_url: relay.clone(),
            kick_api_url: relay.clone(),
        };
        let dcfg = rd::DiscordConfig::from_values(
            Some("1561463863197835294".into()),
            Some("bot".into()),
            Some("secret".into()),
            Some(format!("01{}", "00".repeat(31))),
        )
        .unwrap();
        let state = ferrocast_relay::AppState::new(cfg, ferrocast_relay::Db::open(&rdir.path().join("r.db")).unwrap())
            .with_discord(rd::Runtime::new(dcfg, Some(Arc::new(Stand))));
        let relay_db = state.db.clone();
        let rapp = ferrocast_relay::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(rl, rapp).await });
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(ferrocast_secrets::MemorySecrets::default());
        let apps = PlatformApps { relay_url: Some(relay.clone()), ..Default::default() };
        let engine = Engine::start_with(dir.path(), secrets.clone(), apps).await.unwrap();
        let server = serve(engine.clone(), ServeOptions { port: 0, lan: false, dev_origins: vec![] }).await.unwrap();
        let (base, token) = (server.base_url(), server.sessions.desktop_token().to_string());
        let http = reqwest::Client::new();
        let call = |m: Method, path: &str, body: Option<Value>| {
            let mut r = http.request(m, format!("{base}/api/v1{path}")).bearer_auth(&token);
            if let Some(b) = body {
                r = r.json(&b);
            }
            async move {
                let res = r.send().await.unwrap();
                let s = res.status().as_u16();
                (s, res.json::<Value>().await.unwrap_or(Value::Null))
            }
        };
        // No Pro: the relay's 402.
        let (s, v) = call(Method::GET, "/pro/discord", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{v}");
        let auth = secrets.get("relay-install").unwrap().expect("registered");
        let install = auth.split('.').next().unwrap().to_string();
        relay_db.add_pro_sub("sub_d", "monthly", "e", "created", &install).unwrap();
        let synced = Synced {
            status: "active".into(),
            current_end: Some(ferrocast_relay::now_ms() + 30 * 86_400_000),
            cancel_at_end: None,
        };
        relay_db.sync_pro_sub(None, "sub_d", &synced).unwrap();
        let (s, v) = call(Method::GET, "/pro/discord", None).await;
        assert_eq!((s, v["connected"].as_bool()), (200, Some(false)), "{v}");
        let (s, v) =
            call(Method::PUT, "/pro/discord/channel", Some(json!({ "channel_id": "900000000000000010" }))).await;
        assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_connected")), "{v}");
        let (s, v) = call(Method::POST, "/pro/discord/connect", None).await;
        assert_eq!(s, 201, "{v}");
        let invite = reqwest::Url::parse(v["invite_url"].as_str().unwrap()).unwrap();
        let state = invite.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        let page = http
            .get(format!("{relay}/discord/callback"))
            .query(&[("code", "c"), ("state", &state)])
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let (s, v) =
            call(Method::PUT, "/pro/discord/channel", Some(json!({ "channel_id": "900000000000000010" }))).await;
        assert_eq!((s, v["channel_id"].as_str()), (200, Some("900000000000000010")), "{v}");
        let (s, v) = call(Method::PUT, "/pro/discord/go-live", Some(json!({ "enabled": true }))).await;
        assert_eq!((s, v["go_live"]["enabled"].as_bool()), (200, Some(true)));
        let (s, v) = call(Method::GET, "/discord", None).await;
        assert_eq!(
            (s, v["pro_go_live"].as_bool()),
            (200, Some(true)),
            "the free form says the bot posts as soon as it's switched on: {v}"
        );
        // A role above Ferrocast's: the relay's 403 and hint reach the dashboard as they are.
        let rule = json!({ "kind": "sub", "role_id": "900000000000000022" });
        let (s, v) = call(Method::PUT, "/pro/discord/rules", Some(rule)).await;
        assert_eq!((s, v["error"]["code"].as_str()), (403, Some("missing_permission")), "{v}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("Server Settings › Roles"));
        let (s, v) =
            call(Method::PUT, "/pro/discord/rules", Some(json!({ "kind": "sub", "role_id": "900000000000000020" })))
                .await;
        assert_eq!(s, 201, "{v}");
        let id = v["rules"][0]["id"].as_str().unwrap().to_string();
        let (s, _) = call(
            Method::PUT,
            &format!("/pro/discord/rules/{id}"),
            Some(json!({ "kind": "sub", "role_id": "900000000000000020", "keep_days": 7 })),
        )
        .await;
        assert_eq!(s, 200);
        // A malformed rule id never reaches the relay.
        let (s, v) = call(Method::DELETE, "/pro/discord/rules/..%2Fdiscord", None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (404, Some("rule_not_found")));
        let (s, _) = call(Method::DELETE, &format!("/pro/discord/rules/{id}"), None).await;
        assert_eq!(s, 204);
        let (s, v) = call(Method::PUT, "/pro/discord/go-live", Some(json!([1]))).await;
        assert_eq!(s, 400, "{v}");
        let (s, _) = call(Method::DELETE, "/pro/discord", None).await;
        assert_eq!(s, 204);
        let (s, v) = call(Method::GET, "/pro/discord", None).await;
        assert_eq!((s, v["connected"].as_bool()), (200, Some(false)));
        let (s, v) = call(Method::GET, "/discord", None).await;
        assert_eq!((s, v["pro_go_live"].as_bool()), (200, Some(false)), "disconnected: the webhook posts again");
    }
}
