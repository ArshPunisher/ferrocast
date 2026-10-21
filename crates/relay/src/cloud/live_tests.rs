//! Cloud mode live (docs/CLOUD.md M3) through the real router: overlay pages on the overlay
//! origin only, the socket's snapshot, signed Twitch and Kick events becoming alerts and chat,
//! acks moving the queue, rotation and deletes closing pages, test mode, the socket caps,
//! sleeping accounts, and media with byte ranges.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::Method;
use rsa::sha2::Sha256;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::store::Channel;
use super::tests::{KICK_CHANNEL, User, World, kick_key, world};
use crate::bot::tests::{ES_SECRET, STREAMER_ID};
use crate::now_ms;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const OVERLAY_HOST: &str = "overlays.test";

impl World {
    /// The account's first overlay: id and token.
    async fn first_overlay(&self, u: &User) -> (String, String) {
        let (_, list) = self.web(u, Method::GET, "/c/api/v1/overlays", None).await;
        let url = list[0]["url"].as_str().unwrap();
        let token = url.strip_prefix("http://overlays.test/o/").unwrap().trim_end_matches('/').to_string();
        (list[0]["id"].as_str().unwrap().to_string(), token)
    }

    /// A GET on the overlay origin.
    async fn overlay_get(&self, path: &str) -> reqwest::Response {
        self.http.get(format!("{}{path}", self.base)).header("host", OVERLAY_HOST).send().await.unwrap()
    }

    async fn open_socket(&self, token: &str, query: &str) -> Ws {
        let url = format!("{}/o/{token}/ws{query}", self.base.replacen("http", "ws", 1));
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("host", OVERLAY_HOST.parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    /// Binds the streamer's Twitch and Kick channels to `u` (as a finished connect does).
    fn bind_channels(&self, u: &User) {
        for (platform, id, login) in [("twitch", STREAMER_ID, "asha_live"), ("kick", KICK_CHANNEL, "asha_kick")] {
            self.db
                .cloud_bind_channel(&Channel {
                    platform: platform.into(),
                    channel_id: id.into(),
                    account_id: u.account.id.clone(),
                    login: login.into(),
                    name: Some(login.into()),
                    avatar_url: None,
                    scopes: vec![],
                    event_subs: vec!["sub-1".into()],
                    connected_at: now_ms(),
                    error: None,
                })
                .unwrap();
        }
    }

    async fn twitch_event(&self, msg_id: &str, kind: &str, event: Value) -> u16 {
        let cond = if kind == "channel.raid" {
            json!({ "to_broadcaster_user_id": STREAMER_ID })
        } else {
            json!({ "broadcaster_user_id": STREAMER_ID })
        };
        let body = json!({ "subscription": { "type": kind, "condition": cond }, "event": event }).to_string();
        let ts = rfc3339_now();
        let mut m = Hmac::<Sha256>::new_from_slice(ES_SECRET.as_bytes()).unwrap();
        m.update(msg_id.as_bytes());
        m.update(ts.as_bytes());
        m.update(body.as_bytes());
        let sig =
            format!("sha256={}", m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>());
        self.http
            .post(format!("{}/v1/bot/twitch/eventsub", self.base))
            .header("Twitch-Eventsub-Message-Id", msg_id)
            .header("Twitch-Eventsub-Message-Timestamp", ts)
            .header("Twitch-Eventsub-Message-Signature", sig)
            .header("Twitch-Eventsub-Message-Type", "notification")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn kick_event(&self, msg_id: &str, ty: &str, body: Value) -> u16 {
        use rsa::signature::{SignatureEncoding, Signer};
        let body = body.to_string();
        let ts = rfc3339_now();
        let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(kick_key().clone());
        let sig = signer.sign(format!("{msg_id}.{ts}.{body}").as_bytes());
        let sig = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes());
        self.http
            .post(format!("{}/v1/kick/webhook", self.base))
            .header("Kick-Event-Message-Id", msg_id)
            .header("Kick-Event-Message-Timestamp", ts)
            .header("Kick-Event-Signature", sig)
            .header("Kick-Event-Type", ty)
            .header("Kick-Event-Version", "1")
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

fn rfc3339_now() -> String {
    let ms = now_ms();
    let (y, mo, d) = crate::quota::civil(ms.div_euclid(86_400_000));
    let s = ms.rem_euclid(86_400_000) / 1000;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// The next text frame (5 s at most), as JSON.
async fn next(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("a frame in time");
        match m {
            Some(Ok(WsMessage::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(WsMessage::Close(c))) => return json!({ "type": "closed", "code": c.map(|c| u16::from(c.code)) }),
            Some(Ok(_)) => {}
            other => return json!({ "type": "closed", "code": null, "why": format!("{other:?}") }),
        }
    }
}

/// The next frame of `kind`, skipping others.
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    for _ in 0..20 {
        let f = next(ws).await;
        if f["type"] == kind || f["type"] == "closed" {
            return f;
        }
    }
    panic!("no {kind} frame");
}

async fn closed_with(ws: &mut Ws) -> Option<u16> {
    let f = next_of(ws, "closed").await;
    f["code"].as_u64().map(|c| c as u16)
}

#[tokio::test]
async fn pages_are_served_on_the_overlay_origin_only() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, token) = w.first_overlay(&u).await;
    let res = w.overlay_get(&format!("/o/{token}/")).await;
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-security-policy"].to_str().unwrap().contains("connect-src 'self'"));
    assert!(res.text().await.unwrap().contains("<title>Overlay</title>"));
    let res = w.overlay_get(&format!("/o/{token}/assets/app.js")).await;
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    assert_eq!(w.overlay_get(&format!("/o/{token}/assets/..%2Findex.html")).await.status(), 404);
    assert_eq!(w.overlay_get(&format!("/o/{token}/missing.js")).await.status(), 404);
    // An unknown token: a plain 404, never the build.
    let res = w.overlay_get(&format!("/o/{}/", "0".repeat(40))).await;
    assert_eq!(res.status(), 404);
    assert!(!res.text().await.unwrap().contains("<script"));
    // The web host (it carries the session cookie) never serves overlays.
    let res = w.http.get(format!("{}/o/{token}/", w.base)).send().await.unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn twitch_and_kick_events_alert_and_chat_reaches_the_page() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    w.bind_channels(&u);
    let (oid, token) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token, "").await;
    let snap = next(&mut ws).await;
    assert_eq!(snap["type"], "snapshot");
    assert_eq!(snap["data"]["overlay"]["id"], oid.as_str());
    assert!(snap["data"]["overlay"].get("url").is_none(), "the page never sees its own URL");
    assert_eq!(snap["data"]["state"]["poll"], Value::Null);

    // A Twitch follow: an event and its alert.
    let follow =
        json!({ "user_id": "9", "user_login": "nova", "user_name": "Nova", "followed_at": "2026-10-21T10:00:00Z" });
    assert_eq!(w.twitch_event("m-1", "channel.follow", follow.clone()).await, 204);
    let alert = next_of(&mut ws, "alert").await;
    assert_eq!(alert["data"]["kind"], "follow");
    assert!(alert["data"]["title"].as_str().unwrap().contains("Nova"), "{alert}");
    let alert_id = alert["data"]["alert_id"].as_str().unwrap().to_string();
    // The same notification again (a retry with a new id): deduped by the platform's data.
    // Twitch chat for the chat widget.
    let chat = json!({
        "broadcaster_user_id": STREAMER_ID, "chatter_user_id": "7", "chatter_user_login": "viewer",
        "chatter_user_name": "Viewer", "message_id": "chat-1", "message": { "text": "hello cloud", "fragments": [] },
        "badges": [], "color": "", "message_type": "text"
    });
    assert_eq!(w.twitch_event("m-2", "channel.chat.message", chat).await, 204);
    let ev = loop {
        let f = next_of(&mut ws, "event").await;
        if f["data"]["kind"]["type"] != "follow" {
            break f;
        }
    };
    assert_eq!(ev["data"]["kind"]["type"], "chat_message", "{ev}");
    // A Kick follow, while the Twitch one plays: it waits in the queue.
    let kick_follow = json!({ "broadcaster": { "user_id": 668 }, "follower": { "user_id": 5, "username": "kicky" } });
    assert_eq!(w.kick_event("k-1", "channel.followed", kick_follow).await, 200);
    // The page says the Twitch alert finished: the Kick one plays next.
    let ack = json!({ "v": 1, "type": "ack", "data": { "alert_id": alert_id, "phase": "finished" } });
    ws.send(WsMessage::Text(ack.to_string().into())).await.unwrap();
    let next_alert = next_of(&mut ws, "alert").await;
    assert_eq!(next_alert["data"]["platform"], "kick", "{next_alert}");

    // The snapshot of a page opened now has the events and the chat line.
    let mut ws2 = w.open_socket(&token, "").await;
    let snap = next(&mut ws2).await;
    let events = snap["data"]["state"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "{snap}");
    assert!(events.iter().all(|e| e.get("raw").is_none()));
    assert_eq!(snap["data"]["state"]["chat"].as_array().unwrap().len(), 1);
    assert_eq!(snap["data"]["state"]["labels"], json!({}));

    // A deleted message comes off the chat.
    let del = json!({ "broadcaster_user_id": STREAMER_ID, "target_user_id": "7", "target_user_login": "viewer",
                      "target_user_name": "Viewer", "message_id": "chat-1" });
    assert_eq!(w.twitch_event("m-3", "channel.chat.message_delete", del).await, 204);
    let d = next_of(&mut ws2, "delete_message").await;
    assert_eq!(d["data"]["message_id"], "chat-1");
    let bus = w.state.cloud.buses.get(&u.account.id).unwrap();
    assert!(bus.state().chat.is_empty());
}

#[tokio::test]
async fn forged_frames_are_ignored_and_big_ones_close_the_socket() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    w.bind_channels(&u);
    let (_, token) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token, "").await;
    next(&mut ws).await;
    // An overlay can never create events (R-36): an "event" from the page does nothing.
    let forged = json!({ "v": 1, "type": "event", "data": { "kind": { "type": "tip" } } });
    ws.send(WsMessage::Text(forged.to_string().into())).await.unwrap();
    let ack = json!({ "v": 1, "type": "ack", "data": { "alert_id": "not-playing", "phase": "finished" } });
    ws.send(WsMessage::Text(ack.to_string().into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bus = w.state.cloud.buses.get(&u.account.id).unwrap();
    assert!(bus.state().events.is_empty());
    assert!(bus.state().queue.is_empty());
    ws.send(WsMessage::Text("x".repeat(5_000).into())).await.ok();
    assert!(matches!(closed_with(&mut ws).await, None | Some(1009)), "an oversized frame ends the socket");
}

#[tokio::test]
async fn rotating_or_deleting_closes_open_pages_with_4404() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (oid, token) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token, "").await;
    next(&mut ws).await;
    // A layout change reaches the page without a reload.
    let (_, view) = w.web(&u, Method::GET, &format!("/c/api/v1/overlays/{oid}"), None).await;
    let mut edit = view.clone();
    edit["name"] = json!("Renamed");
    w.web(&u, Method::PUT, &format!("/c/api/v1/overlays/{oid}"), Some(edit)).await;
    let cfg = next_of(&mut ws, "config").await;
    assert_eq!(cfg["data"]["name"], "Renamed");

    let (s, _) = w.web(&u, Method::POST, &format!("/c/api/v1/overlays/{oid}/regenerate-url"), None).await;
    assert_eq!(s, 200);
    assert_eq!(next_of(&mut ws, "revoked").await["type"], "revoked");
    assert_eq!(closed_with(&mut ws).await, Some(4404));
    // The old URL's socket and page are gone.
    let mut old = w.open_socket(&token, "").await;
    assert_eq!(closed_with(&mut old).await, Some(4404));
    assert_eq!(w.overlay_get(&format!("/o/{token}/")).await.status(), 404);

    let (_, token2) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token2, "").await;
    next(&mut ws).await;
    w.web(&u, Method::DELETE, &format!("/c/api/v1/overlays/{oid}"), None).await;
    assert_eq!(next_of(&mut ws, "revoked").await["type"], "revoked");
    assert_eq!(closed_with(&mut ws).await, Some(4404));
}

#[tokio::test]
async fn test_mode_sends_samples_and_nothing_real() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, token) = w.first_overlay(&u).await;
    let mut ws = w.open_socket(&token, "?test").await;
    let snap = next(&mut ws).await;
    assert_eq!(snap["data"]["state"]["events"], json!([]));
    let ev = next_of(&mut ws, "event").await;
    assert_eq!(ev["data"]["origin"], "test");
    let alert = next_of(&mut ws, "alert").await;
    assert!(alert["data"]["alert_id"].as_str().unwrap().starts_with("preview-"));
    let bus = w.state.cloud.buses.get(&u.account.id).unwrap();
    assert!(bus.state().queue.is_empty(), "previews never touch the real queue");
}

#[tokio::test]
async fn socket_caps_sleeping_accounts_and_unknown_tokens() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, token) = w.first_overlay(&u).await;
    let mut open = Vec::new();
    for _ in 0..super::bus::SOCKETS_PER_OVERLAY {
        let mut ws = w.open_socket(&token, "").await;
        assert_eq!(next(&mut ws).await["type"], "snapshot");
        open.push(ws);
    }
    let mut extra = w.open_socket(&token, "").await;
    assert_eq!(closed_with(&mut extra).await, Some(4429));
    assert_eq!(w.state.cloud.buses.sockets.load(std::sync::atomic::Ordering::Relaxed), 4);
    drop(open);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.state.cloud.buses.sockets.load(std::sync::atomic::Ordering::Relaxed), 0);

    let mut unknown = w.open_socket(&"a".repeat(40), "").await;
    assert_eq!(closed_with(&mut unknown).await, Some(4404));

    w.db.c().execute("UPDATE cloud_accounts SET sleeping_at = 1", []).unwrap();
    let mut asleep = w.open_socket(&token, "").await;
    assert_eq!(closed_with(&mut asleep).await, Some(4410));
}

#[tokio::test]
async fn media_uploads_are_checked_by_content_and_served_with_ranges() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let png: Vec<u8> = [b"\x89PNG\r\n\x1a\n".as_slice(), &[1u8; 2000]].concat();
    let upload = |bytes: Vec<u8>, name: &str| {
        // A multipart body by hand, as a browser's form sends it.
        let b = "ferrocastboundary7";
        let mut body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
        w.http
            .post(format!("{}/c/api/v1/media", w.base))
            .header("cookie", &u.cookie)
            .header("origin", &w.base)
            .header("x-csrf-token", &u.csrf)
            .header("content-type", format!("multipart/form-data; boundary={b}"))
            .body(body)
            .send()
    };
    let res = upload(png.clone(), "spark.png").await.unwrap();
    assert_eq!(res.status(), 201);
    let m: Value = res.json().await.unwrap();
    assert_eq!((m["kind"].as_str(), m["mime"].as_str()), (Some("image"), Some("image/png")));
    let id = m["id"].as_str().unwrap().to_string();
    // The same file again is the same file.
    let again: Value = upload(png.clone(), "copy.png").await.unwrap().json().await.unwrap();
    assert_eq!(again["id"], id.as_str());
    // A page renamed .png: refused by its content.
    let res = upload(b"<html><script>alert(1)</script>".to_vec(), "x.png").await.unwrap();
    assert_eq!(res.status(), 415);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "unsupported_media");
    assert!(v["error"]["hint"].is_string());
    // Over 20 MB for one file.
    let big: Vec<u8> = [b"\x89PNG\r\n\x1a\n".as_slice(), &vec![0u8; 20_000_001]].concat();
    let res = upload(big, "big.png").await.unwrap();
    assert_eq!(res.status(), 413);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "too_large");
    // The account's 200 MB.
    w.db.c()
        .execute(
            "INSERT INTO cloud_media(account_id, id, sha256, kind, mime, bytes, name, created_at) VALUES (?1, 'filler', 'f', 'video', 'video/mp4', 199999000, 'filler', 0)",
            [&u.account.id],
        )
        .unwrap();
    let other: Vec<u8> = [b"GIF89a".as_slice(), &[2u8; 5000]].concat();
    let res = upload(other, "x.gif").await.unwrap();
    assert_eq!(res.status(), 413);
    let v: Value = res.json().await.unwrap();
    assert_eq!(v["error"]["code"], "cloud_storage_full");
    assert!(v["error"]["hint"].as_str().unwrap().contains("media library"));

    // Served to the account's overlays, with byte ranges.
    let (_, token) = w.first_overlay(&u).await;
    let res = w.overlay_get(&format!("/o/{token}/media/{id}")).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    assert_eq!(res.bytes().await.unwrap().len(), png.len());
    let res = w
        .http
        .get(format!("{}/o/{token}/media/{id}", w.base))
        .header("host", OVERLAY_HOST)
        .header("range", "bytes=0-7")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 206);
    assert_eq!(res.headers()["content-range"], format!("bytes 0-7/{}", png.len()).as_str());
    assert_eq!(&res.bytes().await.unwrap()[..], b"\x89PNG\r\n\x1a\n");
    // Another account's overlay can't reach it.
    let b = w.cloud_user("b@example.com").await;
    let (_, tb) = w.first_overlay(&b).await;
    assert_eq!(w.overlay_get(&format!("/o/{tb}/media/{id}")).await.status(), 404);

    // In use by a rule: refused; then deleted.
    let (_, rules) = w.web(&u, Method::GET, "/c/api/v1/alert-rules", None).await;
    let mut r = rules[0].clone();
    r["variations"][0]["image"] = json!(id);
    let rid = r["id"].as_str().unwrap().to_string();
    let (s, v) = w.web(&u, Method::PUT, &format!("/c/api/v1/alert-rules/{rid}"), Some(r.clone())).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = w.web(&u, Method::DELETE, &format!("/c/api/v1/media/{id}"), None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("in_use")));
    r["variations"][0]["image"] = Value::Null;
    w.web(&u, Method::PUT, &format!("/c/api/v1/alert-rules/{rid}"), Some(r)).await;
    let (s, _) = w.web(&u, Method::DELETE, &format!("/c/api/v1/media/{id}"), None).await;
    assert_eq!(s, 204);
    assert_eq!(w.overlay_get(&format!("/o/{token}/media/{id}")).await.status(), 404);
}

#[test]
fn tips_map_like_the_desktop_and_only_captured_ones() {
    let p = json!({ "provider": "razorpay", "status": "captured", "payment_id": "pay_1", "amount_minor": 50000,
                    "digits": 2, "currency": "INR", "name": "Asha", "message": "Keep it up!" });
    let e = super::ingest::tip_event(&p).unwrap();
    assert_eq!(e.platform, ferrocast_core::event::Platform::Tips);
    assert!(e.kind.is_alertable());
    assert_eq!(e.source_id.as_deref(), Some("razorpay:pay_1"));
    let mut pending = p.clone();
    pending["status"] = json!("pending");
    assert!(super::ingest::tip_event(&pending).is_none());
    let mut bad = p;
    bad["provider"] = json!("venmo");
    assert!(super::ingest::tip_event(&bad).is_none());
}

impl World {
    /// The dashboard's live socket for `u` (cookie, web host, its own Origin).
    async fn open_live(&self, u: &User, origin: &str) -> Result<Ws, u16> {
        let url = format!("{}/c/api/v1/live", self.base.replacen("http", "ws", 1));
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("cookie", u.cookie.parse().unwrap());
        req.headers_mut().insert("origin", origin.parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
            Err(e) => panic!("{e}"),
        }
    }
}

#[tokio::test]
async fn test_alerts_the_queue_and_the_dashboards_live_socket() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    let (_, token) = w.first_overlay(&u).await;
    let mut page = w.open_socket(&token, "").await;
    next(&mut page).await;
    // Another site can't open the dashboard's socket.
    assert_eq!(w.open_live(&u, "https://evil.example").await.err(), Some(403));
    let mut dash = w.open_live(&u, &w.base.clone()).await.unwrap();
    assert_eq!(next(&mut dash).await["type"], "hello");
    let status = next(&mut dash).await;
    assert_eq!(status["type"], "status");
    assert_eq!(status["data"]["connectors"]["twitch"]["state"], "off");
    assert_eq!(next(&mut dash).await["type"], "queue");

    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "tip", "amount": 250 }))).await;
    assert_eq!((s, &v), (202, &json!({ "ok": true })));
    let alert = next_of(&mut page, "alert").await;
    assert_eq!(alert["data"]["kind"], "tip");
    assert_eq!(alert["data"]["test"], true);
    assert!(alert["data"]["title"].as_str().unwrap().contains("250"), "{alert}");
    // The queue changes first (the alert is queued as the event goes through), then the event.
    assert_eq!(next_of(&mut dash, "queue").await["data"]["playing"]["alert"]["kind"], "tip");
    let ev = next_of(&mut dash, "event").await;
    assert_eq!(ev["data"]["origin"], "test");
    // Test events are never in the recent list (only live ones are).
    let (_, list) = w.web(&u, Method::GET, "/c/api/v1/events", None).await;
    assert_eq!(list["events"], json!([]));

    // Pause, skip, resume.
    let (s, q) = w.web(&u, Method::POST, "/c/api/v1/queue/pause", None).await;
    assert_eq!((s, q["paused"].as_bool()), (200, Some(true)));
    let (_, q) = w.web(&u, Method::POST, "/c/api/v1/queue/skip", None).await;
    assert_eq!(q["playing"], Value::Null);
    assert_eq!(next_of(&mut page, "alert_hide").await["type"], "alert_hide");
    let (_, q) = w.web(&u, Method::POST, "/c/api/v1/queue/resume", None).await;
    assert_eq!(q["paused"], false);
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/queue/explode", None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));

    // Bad tests name the field; then the 10 a minute.
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "nonsense" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid")));
    assert!(v["error"]["message"].as_str().unwrap().starts_with("kind:"));
    let (s, _) = w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "follow", "amount": 5 }))).await;
    assert_eq!(s, 422, "a follow has no amount");
    let mut last = 0;
    for _ in 0..10 {
        last = w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "follow" }))).await.0;
    }
    assert_eq!(last, 429);
}

#[tokio::test]
async fn the_designers_test_uses_its_design_and_chat_waits_for_a_viewer() {
    let w = world().await;
    let u = w.cloud_user("a@example.com").await;
    w.bind_channels(&u);
    let (_, token) = w.first_overlay(&u).await;
    let mut page = w.open_socket(&token, "").await;
    next(&mut page).await;
    let (s, _) = w
        .web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "follow", "design_id": "builtin.molten" })))
        .await;
    assert_eq!(s, 202);
    let alert = next_of(&mut page, "alert").await;
    assert_eq!(alert["data"]["design"]["design_id"], "builtin.molten", "{alert}");
    let (s, v) =
        w.web(&u, Method::POST, "/c/api/v1/test", Some(json!({ "kind": "follow", "design_id": "builtin.brb" }))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (422, Some("invalid")));

    // The Starter overlay has a chat widget, so chat is kept; with no page open it isn't.
    drop(page);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let chat = json!({ "broadcaster_user_id": STREAMER_ID, "chatter_user_id": "7", "chatter_user_login": "v",
                       "chatter_user_name": "V", "message_id": "c-9", "message": { "text": "anyone?", "fragments": [] } });
    w.twitch_event("m-9", "channel.chat.message", chat).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, lines) = w.web(&u, Method::GET, "/c/api/v1/chat", None).await;
    assert_eq!(lines, json!([]));
}

#[tokio::test]
async fn the_editors_lists_and_a_status_before_turning_on() {
    let w = world().await;
    let off = w.user("off@example.com");
    let (s, v) = w.web(&off, Method::GET, "/c/api/v1/status", None).await;
    assert_eq!((s, v["cloud"].as_bool()), (200, Some(true)));
    let u = w.cloud_user("a@example.com").await;
    let (s, v) = w.web(&u, Method::GET, "/c/api/v1/themes", None).await;
    assert_eq!(s, 200);
    assert_eq!(v.as_array().unwrap().len(), 5);
    assert_eq!(v[0]["id"], "molten");
    let (_, v) = w.web(&u, Method::GET, "/c/api/v1/goals", None).await;
    assert_eq!(v, json!([]));
    let (s, v) = w.web(&u, Method::POST, "/c/api/v1/queue/nope/remove", None).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
}
