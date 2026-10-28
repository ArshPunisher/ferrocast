//! Tests against fakes: Google's token endpoint, the REST API, and a scripted gRPC
//! `streamList` server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use ferrocast_core::event::{EventKind, Millis, Origin};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};
use tonic::{Request, Response, Status};

use super::*;
use crate::proto::live_chat_message_snippet::DisplayedContent;
use crate::proto::live_chat_message_snippet::type_wrapper::Type;
use crate::proto::v3_data_live_chat_message_service_server::{
    V3DataLiveChatMessageService, V3DataLiveChatMessageServiceServer,
};
use crate::proto::*;

fn msg(id: &str, ty: Type, content: Option<DisplayedContent>, author: &str) -> LiveChatMessage {
    LiveChatMessage {
        kind: None,
        etag: None,
        id: Some(id.into()),
        snippet: Some(LiveChatMessageSnippet {
            r#type: Some(ty as i32),
            live_chat_id: Some("chat-1".into()),
            author_channel_id: Some(author.into()),
            published_at: None,
            has_display_content: Some(true),
            display_message: None,
            displayed_content: content,
        }),
        author_details: Some(LiveChatMessageAuthorDetails {
            channel_id: Some(author.into()),
            channel_url: None,
            display_name: Some(format!("{author} name")),
            profile_image_url: None,
            is_verified: Some(false),
            is_chat_owner: Some(false),
            is_chat_sponsor: Some(true),
            is_chat_moderator: Some(false),
        }),
    }
}

fn text(id: &str, t: &str) -> LiveChatMessage {
    msg(
        id,
        Type::TextMessageEvent,
        Some(DisplayedContent::TextMessageDetails(LiveChatTextMessageDetails { message_text: Some(t.into()) })),
        "UCviewer",
    )
}

fn page(items: Vec<LiveChatMessage>, next: &str, offline: bool) -> LiveChatMessageListResponse {
    LiveChatMessageListResponse {
        kind: None,
        etag: None,
        offline_at: offline.then(|| "2026-05-01T10:00:00Z".to_string()),
        page_info: None,
        next_page_token: Some(next.into()),
        items,
        active_poll_item: None,
    }
}

#[test]
fn super_chat_micros_become_exact_minor_units() {
    assert_eq!(map::micros_to_money(1_750_000, "usd"), ferrocast_core::money::Money::new(175, "USD"));
    assert_eq!(map::micros_to_money(199_000_000, "INR"), ferrocast_core::money::Money::new(19_900, "INR"));
    assert_eq!(map::micros_to_money(1_000_000_000, "JPY"), ferrocast_core::money::Money::new(1000, "JPY"));
    assert_eq!(map::micros_to_money(1_234_000, "KWD"), ferrocast_core::money::Money::new(1234, "KWD"));
    assert_eq!(
        map::micros_to_money(u64::MAX, "USD").minor,
        i64::try_from(u128::from(u64::MAX) * 100 / 1_000_000).unwrap()
    );
}

#[test]
fn maps_every_kind_we_show() {
    let at = Millis(1);
    let sc = msg(
        "sc",
        Type::SuperChatEvent,
        Some(DisplayedContent::SuperChatDetails(LiveChatSuperChatDetails {
            amount_micros: Some(5_000_000),
            currency: Some("USD".into()),
            amount_display_string: None,
            user_comment: Some("gg".into()),
            tier: Some(2),
        })),
        "UCa",
    );
    let e = map::map(&sc, at).unwrap();
    assert!(
        matches!(&e.kind, EventKind::PaidMessage { money, content: Some(c), .. } if money.minor == 500 && c.text == "gg")
    );
    let ms = msg(
        "ms",
        Type::MemberMilestoneChatEvent,
        Some(DisplayedContent::MemberMilestoneChatDetails(LiveChatMemberMilestoneChatDetails {
            member_level_name: Some("Gold".into()),
            member_month: Some(12),
            user_comment: None,
        })),
        "UCb",
    );
    assert!(matches!(map::map(&ms, at).unwrap().kind, EventKind::Subscription { months: 12, .. }));
    let gifting = msg(
        "g",
        Type::MembershipGiftingEvent,
        Some(DisplayedContent::MembershipGiftingDetails(LiveChatMembershipGiftingDetails {
            gift_memberships_count: Some(5),
            gift_memberships_level_name: None,
        })),
        "UCc",
    );
    assert!(matches!(map::map(&gifting, at).unwrap().kind, EventKind::GiftBatch { count: 5, .. }));
    let got = msg(
        "r",
        Type::GiftMembershipReceivedEvent,
        Some(DisplayedContent::GiftMembershipReceivedDetails(LiveChatGiftMembershipReceivedDetails {
            member_level_name: None,
            gifter_channel_id: Some("UCc".into()),
            associated_membership_gifting_message_id: None,
        })),
        "UCd",
    );
    assert!(!map::map(&got, at).unwrap().kind.is_alertable(), "recipients don't alert one by one");
    let tomb = msg("deleted-1", Type::Tombstone, None, "UCe");
    let t = map::map(&tomb, at).unwrap();
    assert!(matches!(&t.kind, EventKind::MessageDeleted { message_id } if message_id == "deleted-1"));
    assert_ne!(t.source_id.as_deref(), Some("deleted-1"), "the tombstone gets its own dedupe key");
    let text_msg = map::map(&text("t1", "hello"), at).unwrap();
    assert!(
        matches!(&text_msg.kind, EventKind::ChatMessage { actor, .. } if actor.roles.contains(ferrocast_core::event::Roles::MEMBER))
    );
    assert!(map::map(&msg("p", Type::PollEvent, None, "UCf"), at).is_none());
    let mut bad = text("x", "y");
    bad.snippet = None;
    assert!(map::map(&bad, at).is_none(), "a message without a snippet is skipped, not a panic");
}

// --- fakes -------------------------------------------------------------------

#[derive(Default)]
struct Script {
    requests: Vec<Option<String>>,
}

struct FakeChat {
    script: Arc<Mutex<Script>>,
}

#[tonic::async_trait]
impl V3DataLiveChatMessageService for FakeChat {
    type StreamListStream = tokio_stream::wrappers::ReceiverStream<Result<LiveChatMessageListResponse, Status>>;

    async fn stream_list(
        &self,
        req: Request<LiveChatMessageListRequest>,
    ) -> Result<Response<Self::StreamListStream>, Status> {
        if req.metadata().get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer access-0") {
            return Err(Status::unauthenticated("bad token"));
        }
        let token = req.into_inner().page_token;
        let n = {
            let mut s = self.script.lock().unwrap();
            s.requests.push(token);
            s.requests.len()
        };
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            if n == 1 {
                let _ = tx.send(Ok(page(vec![text("old-1", "!hi from before")], "p1", false))).await;
                let _ = tx.send(Ok(page(vec![text("new-1", "fresh")], "p2", false))).await;
                // then the server ends the stream, as YouTube does from time to time
            } else {
                let _ = tx.send(Ok(page(vec![text("new-2", "after the drop")], "p3", false))).await;
                let _ = tx.send(Ok(page(vec![], "p4", true))).await;
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

async fn fakes() -> (Endpoints, Arc<Mutex<Script>>, Arc<Mutex<Vec<String>>>) {
    let script = Arc::new(Mutex::new(Script::default()));
    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = grpc.local_addr().unwrap();
    let svc = V3DataLiveChatMessageServiceServer::new(FakeChat { script: script.clone() });
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc))
            .await
    });
    let forms = Arc::new(Mutex::new(Vec::new()));
    let f2 = forms.clone();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(move |body: String| {
                let f = f2.clone();
                async move {
                    f.lock().unwrap().push(body);
                    axum::Json(json!({ "access_token": "access-0", "refresh_token": "refresh-0", "expires_in": 3600 }))
                }
            }),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(|| async {
                axum::Json(json!({ "items": [{ "snippet": { "liveChatId": "chat-1" } }] }))
            }),
        );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, app).await });
    let ep = Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: format!("http://{grpc_addr}"),
    };
    (ep, script, forms)
}

fn app() -> AppCredentials {
    AppCredentials { client_id: "cid".into(), client_secret: "csecret".into() }
}

#[tokio::test]
async fn sign_in_checks_state_and_sends_the_pkce_verifier() {
    let (ep, _s, forms) = fakes().await;
    let http = http_client();
    let pending = oauth::begin(&ep, &app()).await.unwrap();
    let url = url::Url::parse(&pending.url).unwrap();
    let q = |k: &str| url.query_pairs().find(|(a, _)| a == k).map(|(_, v)| v.into_owned()).unwrap();
    let (redirect, state, challenge) = (q("redirect_uri"), q("state"), q("code_challenge"));
    assert_eq!(q("code_challenge_method"), "S256");
    let browser = tokio::spawn({
        let http = http.clone();
        async move {
            let _ = http.get(format!("{redirect}?code=the-code&state={state}")).send().await;
        }
    });
    let tokens = oauth::finish(&http, &ep, &app(), pending, Duration::from_secs(5)).await.unwrap();
    browser.await.unwrap();
    assert_eq!(tokens.refresh_token, "refresh-0");
    let body = forms.lock().unwrap()[0].clone();
    let verifier = body.split('&').find_map(|kv| kv.strip_prefix("code_verifier=")).unwrap();
    let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(expect, challenge, "the verifier matches the challenge Google saw");
}

#[tokio::test]
async fn a_forged_redirect_with_the_wrong_state_is_refused() {
    let (ep, _s, _f) = fakes().await;
    let http = http_client();
    let pending = oauth::begin(&ep, &app()).await.unwrap();
    let redirect =
        url::Url::parse(&pending.url).unwrap().query_pairs().find(|(k, _)| k == "redirect_uri").unwrap().1.into_owned();
    let h = http.clone();
    tokio::spawn(async move {
        let _ = h.get(format!("{redirect}?code=evil&state=guessed")).send().await;
    });
    let err = oauth::finish(&http, &ep, &app(), pending, Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, YoutubeError::SignIn(_)));
}

#[tokio::test]
async fn the_stream_marks_history_resumes_after_a_drop_and_ends_with_the_chat() {
    let (ep, script, _f) = fakes().await;
    let tokens =
        Tokens { access_token: "access-0".into(), refresh_token: "r".into(), expires_at: now_ms() + 3_600_000 };
    let api = Arc::new(Api::new(http_client(), ep, app(), tokens, Arc::new(|_| Ok(()))));
    let (sink, mut rx) = mpsc::channel(64);
    let live: stream::LiveChat = Arc::default();
    tokio::spawn(stream::run(api, sink, Arc::new(Notify::new()), live.clone()));
    let mut chat = Vec::new();
    let mut saw_online = false;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        match e.kind {
            EventKind::StreamOnline { .. } => saw_online = true,
            EventKind::ChatMessage { .. } => chat.push((e.source_id.unwrap(), e.origin)),
            EventKind::StreamOffline => break,
            _ => {}
        }
    }
    assert!(saw_online);
    assert_eq!(
        chat,
        vec![("old-1".into(), Origin::Backfill), ("new-1".into(), Origin::Live), ("new-2".into(), Origin::Live)]
    );
    let requests = script.lock().unwrap().requests.clone();
    assert_eq!(requests, vec![None, Some("p2".into())], "resumed from the last page token");
    assert!(live.read().unwrap().is_none(), "no live chat after it ended");
}

// ---------------------------------------------------------------- stream info (Studio)

/// A fake YouTube Data API for the broadcast's video: `scheduled` decides whether there's
/// an upcoming broadcast; PUT /videos bodies are recorded.
async fn fake_videos(scheduled: bool) -> (Endpoints, Arc<Mutex<Vec<serde_json::Value>>>) {
    use axum::extract::Query;
    use std::collections::HashMap;
    let puts = Arc::new(Mutex::new(Vec::new()));
    let p2 = puts.clone();
    let p3 = puts.clone();
    let app = axum::Router::new()
        // thumbnails.set: a body of "403" is refused like an unverified channel; anything
        // else is recorded as {"thumbnail": video id, "type", "bytes"}.
        .route(
            "/api/upload/thumbnails/set",
            axum::routing::post(
                move |Query(q): Query<HashMap<String, String>>, h: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let p = p3.clone();
                    async move {
                        if &body[..] == b"403" {
                            return (
                                axum::http::StatusCode::FORBIDDEN,
                                axum::Json(json!({ "error": { "message": "The authenticated user doesn't have permissions to upload and set custom video thumbnails.", "errors": [{ "reason": "forbidden" }] } })),
                            );
                        }
                        p.lock().unwrap().push(json!({
                            "thumbnail": q.get("videoId"),
                            "type": h.get("content-type").and_then(|v| v.to_str().ok()),
                            "bytes": body.len(),
                        }));
                        (axum::http::StatusCode::OK, axum::Json(json!({ "items": [] })))
                    }
                },
            ),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| async move {
                let upcoming = q.get("broadcastStatus").map(String::as_str) == Some("upcoming");
                let items = if upcoming && scheduled { json!([{ "id": "vid-1" }]) } else { json!([]) };
                axum::Json(json!({ "items": items }))
            }),
        )
        .route(
            "/api/liveStreams",
            axum::routing::get(|Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q.get("mine").map(String::as_str), Some("true"));
                axum::Json(json!({ "items": [{ "id": "s1", "snippet": { "isDefaultStream": true },
                    "cdn": { "ingestionInfo": { "streamName": "abcd-efgh-ijkl",
                        "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" } } }] }))
            }),
        )
        .route(
            "/api/videos",
            axum::routing::get(|| async {
                axum::Json(json!({ "items": [{ "id": "vid-1", "snippet": {
                    "title": "Old", "description": "Keep me", "categoryId": "24", "channelId": "UC1",
                    "publishedAt": "2026-01-01T00:00:00Z", "thumbnails": {}, "localized": {} } }] }))
            })
            .put(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let p = p2.clone();
                async move {
                    p.lock().unwrap().push(body.clone());
                    axum::Json(body)
                }
            }),
        );
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, app).await });
    let ep = Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: "http://127.0.0.1:9".into(),
    };
    (ep, puts)
}

fn video_api(ep: Endpoints) -> crate::api::Api {
    let tokens =
        oauth::Tokens { access_token: "a".into(), refresh_token: "r".into(), expires_at: now_ms() + 3_600_000 };
    crate::api::Api::new(http_client(), ep, app(), tokens, Arc::new(|_| Ok(())))
}

#[tokio::test]
async fn stream_info_updates_the_broadcast_video_and_keeps_its_description() {
    use ferrocast_core::stream_info::{Category, CategoryIds, StreamInfo};
    let (ep, puts) = fake_videos(true).await;
    let api = video_api(ep);
    let info = StreamInfo {
        title: Some("Ranked grind".into()),
        category: Some(Category { name: "Valorant".into(), ids: CategoryIds::default() }),
        tags: Some(vec!["valorant".into()]),
        language: Some("hi".into()),
        ..Default::default()
    };
    assert_eq!(api.update_broadcast_info(&info).await.unwrap(), vec!["title", "category", "tags", "language"]);
    let body = puts.lock().unwrap()[0].clone();
    assert_eq!(body["id"], "vid-1");
    assert_eq!(body["snippet"]["title"], "Ranked grind");
    assert_eq!(body["snippet"]["description"], "Keep me", "videos.update would wipe it otherwise");
    assert_eq!(body["snippet"]["categoryId"], "20", "a game is YouTube's Gaming category");
    assert_eq!(body["snippet"]["defaultAudioLanguage"], "hi");
    assert!(body["snippet"].get("channelId").is_none() && body["snippet"].get("publishedAt").is_none());
    let long = StreamInfo { title: Some("x".repeat(101)), ..Default::default() };
    assert!(api.update_broadcast_info(&long).await.unwrap_err().to_string().contains("100 characters"));
    assert_eq!(crate::api::search_video_categories("gam")[0].id, "20");
    assert_eq!(crate::api::search_video_categories("").len(), crate::api::VIDEO_CATEGORIES.len());
}

#[tokio::test]
async fn no_live_or_scheduled_broadcast_is_a_clear_refusal() {
    use ferrocast_core::stream_info::StreamInfo;
    let (ep, puts) = fake_videos(false).await;
    let err = video_api(ep).update_broadcast_info(&StreamInfo { title: Some("x".into()), ..Default::default() }).await;
    assert!(err.unwrap_err().to_string().contains("live or scheduled stream"));
    assert!(puts.lock().unwrap().is_empty());
}

/// A chat server that sends one page, then keeps the stream open without a word.
struct QuietChat;

#[tonic::async_trait]
impl V3DataLiveChatMessageService for QuietChat {
    type StreamListStream = tokio_stream::wrappers::ReceiverStream<Result<LiveChatMessageListResponse, Status>>;

    async fn stream_list(
        &self,
        _req: Request<LiveChatMessageListRequest>,
    ) -> Result<Response<Self::StreamListStream>, Status> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _ = tx.send(Ok(page(vec![text("q-1", "hello")], "p1", false))).await;
            std::future::pending::<()>().await; // never closes
            drop(tx);
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

/// Copies bytes both ways until `frozen`; then holds both sockets open and silent (a dead
/// network path that never closes).
async fn freezing_proxy(to: std::net::SocketAddr, frozen: Arc<std::sync::atomic::AtomicBool>) -> String {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = l.accept().await {
            let server = tokio::net::TcpStream::connect(to).await.unwrap();
            let (mut cr, mut cw) = client.into_split();
            let (mut sr, mut sw) = server.into_split();
            let f1 = frozen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 16_384];
                while let Ok(n) = cr.read(&mut buf).await {
                    if n == 0 || f1.load(Ordering::SeqCst) {
                        break;
                    }
                    if sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                std::future::pending::<()>().await;
                drop((cr, sw));
            });
            let f2 = frozen.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 16_384];
                while let Ok(n) = sr.read(&mut buf).await {
                    if n == 0 || f2.load(Ordering::SeqCst) {
                        break;
                    }
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                std::future::pending::<()>().await;
                drop((sr, cw));
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_stream_on_a_dead_connection_ends_through_keepalive_instead_of_hanging() {
    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = grpc.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(V3DataLiveChatMessageServiceServer::new(QuietChat))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc))
            .await
    });
    let frozen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let url = freezing_proxy(grpc_addr, frozen.clone()).await;
    let quick = (Duration::from_millis(500), Duration::from_millis(500));
    let mut feed = crate::feed::open_tuned(&url, "access-0", "chat-1", None, quick).await.unwrap();
    assert_eq!(feed.next().await.unwrap().unwrap().events.len(), 1);
    // The path goes dead: no close, no answer to pings. Without keepalive this waits forever.
    frozen.store(true, std::sync::atomic::Ordering::SeqCst);
    let ended = tokio::time::timeout(Duration::from_secs(10), feed.next()).await.expect("keepalive ends it");
    assert!(matches!(ended, Err(crate::feed::FeedError::Lost(_))), "{ended:?}");
    // The production values.
    assert_eq!(
        (crate::feed::KEEPALIVE_INTERVAL, crate::feed::KEEPALIVE_TIMEOUT),
        (Duration::from_secs(30), Duration::from_secs(10))
    );
}

// ---------------------------------------------------------------- the relay's feed: history

/// The first answer holds an old line, one with no time, and one published after the stream
/// opened; the next answer one more.
struct TimedChat;

fn at(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}+00:00",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        ms.rem_euclid(1000)
    )
}

fn text_at(id: &str, t: &str, published: Option<String>) -> LiveChatMessage {
    let mut m = text(id, t);
    if let Some(s) = m.snippet.as_mut() {
        s.published_at = published;
    }
    m
}

#[tonic::async_trait]
impl V3DataLiveChatMessageService for TimedChat {
    type StreamListStream = tokio_stream::wrappers::ReceiverStream<Result<LiveChatMessageListResponse, Status>>;

    async fn stream_list(
        &self,
        _req: Request<LiveChatMessageListRequest>,
    ) -> Result<Response<Self::StreamListStream>, Status> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let now = now_ms();
            let first = vec![
                text_at("old", "from before", Some(at(now - 60_000))),
                text_at("untimed", "no time", None),
                text_at("fresh", "!ferrocast", Some(at(now + 1_000))),
            ];
            let _ = tx.send(Ok(page(first, "p1", false))).await;
            let _ = tx.send(Ok(page(vec![text_at("later", "hi", Some(at(now + 2_000)))], "p2", false))).await;
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

#[test]
fn published_times_parse_with_offsets() {
    assert_eq!(map::published_ms("1970-01-01T00:00:01Z"), Some(1_000));
    assert_eq!(map::published_ms("1970-01-01T01:00:00.250+01:00"), Some(250));
    assert_eq!(map::published_ms("2026-10-21T13:02:11.123456+00:00"), map::published_ms("2026-10-21T13:02:11.123Z"));
    assert_eq!(map::published_ms(&at(1_761_051_731_123)), Some(1_761_051_731_123));
    assert_eq!(map::published_ms("yesterday"), None);
}

#[tokio::test]
async fn only_lines_from_before_the_stream_opened_are_history() {
    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = grpc.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(V3DataLiveChatMessageServiceServer::new(TimedChat))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(grpc))
            .await
    });
    let mut feed = crate::feed::open(&format!("http://{addr}"), "access-0", "chat-1", None).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let b = feed.next().await.unwrap().unwrap();
        seen.extend(b.events.into_iter().map(|e| (e.source_id.unwrap(), e.origin)));
    }
    assert_eq!(
        seen,
        vec![
            ("old".into(), Origin::Backfill),
            ("untimed".into(), Origin::Backfill),
            ("fresh".into(), Origin::Live),
            ("later".into(), Origin::Live),
        ]
    );
}

#[tokio::test]
async fn a_description_replaces_the_old_one_and_a_thumbnail_is_uploaded() {
    use ferrocast_core::stream_info::StreamInfo;
    let (ep, puts) = fake_videos(true).await;
    let api = video_api(ep);
    let info = StreamInfo { description: Some("New words\nline two".into()), ..Default::default() };
    assert_eq!(api.update_broadcast_info(&info).await.unwrap(), vec!["description"]);
    assert_eq!(puts.lock().unwrap()[0]["snippet"]["description"], "New words\nline two");
    // Only a thumbnail: videos.update isn't called.
    let only = StreamInfo { thumbnail: Some("m1".into()), ..Default::default() };
    assert!(api.update_broadcast_info(&only).await.unwrap().is_empty());
    assert_eq!(puts.lock().unwrap().len(), 1);
    api.set_thumbnail(b"\x89PNG fake", "image/png").await.unwrap();
    let up = puts.lock().unwrap()[1].clone();
    assert_eq!(
        (up["thumbnail"].as_str(), up["type"].as_str(), up["bytes"].as_u64()),
        (Some("vid-1"), Some("image/png"), Some(9))
    );
    let e = api.set_thumbnail(b"403", "image/jpeg").await.unwrap_err().to_string();
    assert!(e.contains("verified channel"), "{e}");
    let e = api.set_thumbnail(b"GIF89a", "image/gif").await.unwrap_err().to_string();
    assert!(e.contains("JPEG or PNG"), "{e}");
    let e =
        api.set_thumbnail(&vec![0; crate::api::MAX_THUMBNAIL_BYTES + 1], "image/jpeg").await.unwrap_err().to_string();
    assert!(e.contains("50 MB"), "{e}");
    // No broadcast: the usual refusal.
    let (ep, _) = fake_videos(false).await;
    let e = video_api(ep).set_thumbnail(b"x", "image/png").await.unwrap_err().to_string();
    assert!(e.contains("live or scheduled"), "{e}");
}

// ---------------------------------------------------------------- ingest (Go live everywhere)

#[test]
fn the_bound_stream_wins_then_the_default_then_the_first() {
    use crate::api::pick_stream;
    let s = |id: &str, default: bool, key: &str| {
        json!({ "id": id, "snippet": { "isDefaultStream": default },
                "cdn": { "ingestionInfo": { "streamName": key,
                    "ingestionAddress": "rtmp://a.rtmp.youtube.com/live2",
                    "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2/" } } })
    };
    let list = json!({ "items": [s("one", false, "k-one"), s("dflt", true, "k-dflt"), s("bound", false, "k-bound")] });
    let i = pick_stream(&list, Some("bound")).unwrap();
    assert_eq!((i.key.as_str(), i.url.as_str()), ("k-bound", "rtmps://a.rtmps.youtube.com/live2"));
    assert_eq!(pick_stream(&list, None).unwrap().key, "k-dflt");
    assert_eq!(pick_stream(&list, Some("gone")).unwrap().key, "k-dflt");
    let plain = json!({ "items": [s("one", false, "k-one")] });
    assert_eq!(pick_stream(&plain, None).unwrap().key, "k-one");
    // No secure address: the plain one. No key, or nothing: none.
    let old = json!({ "items": [{ "id": "x", "cdn": { "ingestionInfo": { "streamName": "k", "ingestionAddress": "rtmp://a.rtmp.youtube.com/live2" } } }] });
    assert_eq!(pick_stream(&old, None).unwrap().url, "rtmp://a.rtmp.youtube.com/live2");
    assert!(pick_stream(&json!({ "items": [{ "id": "x", "cdn": {} }] }), None).is_none());
    assert!(pick_stream(&json!({ "items": [] }), None).is_none());
    assert!(pick_stream(&json!({}), None).is_none());
}

#[tokio::test]
async fn ingest_asks_youtube_for_the_channels_stream() {
    let (ep, _) = fake_videos(false).await;
    let i = video_api(ep).ingest().await.unwrap();
    assert_eq!((i.url.as_str(), i.key.as_str()), ("rtmps://a.rtmps.youtube.com/live2", "abcd-efgh-ijkl"));
}

/// A YouTube that keeps the streams and broadcasts it's given (for the vertical stream).
async fn fake_live() -> (Endpoints, Arc<Mutex<Vec<String>>>) {
    use axum::extract::{Query, State};
    use std::collections::HashMap;
    #[derive(Clone, Default)]
    struct Live {
        streams: Arc<Mutex<Vec<serde_json::Value>>>,
        broadcasts: Arc<Mutex<Vec<serde_json::Value>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }
    let st = Live::default();
    st.streams.lock().unwrap().push(json!({ "id": "s1", "snippet": { "title": "Default", "isDefaultStream": true },
        "cdn": { "ingestionInfo": { "streamName": "main-key", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" } } }));
    st.broadcasts.lock().unwrap().push(json!({ "id": "b1", "snippet": { "title": "Day 41 ranked" },
        "status": { "privacyStatus": "unlisted", "lifeCycleStatus": "ready" }, "contentDetails": { "boundStreamId": "s1" } }));
    let calls = st.calls.clone();
    let app = axum::Router::new()
        .route(
            "/api/liveStreams",
            axum::routing::get(|State(st): State<Live>| async move {
                st.calls.lock().unwrap().push("streams.list".into());
                axum::Json(json!({ "items": st.streams.lock().unwrap().clone() }))
            })
            .post(|State(st): State<Live>, axum::Json(mut b): axum::Json<serde_json::Value>| async move {
                st.calls.lock().unwrap().push("streams.insert".into());
                b["id"] = json!("sv");
                b["cdn"]["ingestionInfo"] = json!({ "streamName": "vertical-key", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" });
                st.streams.lock().unwrap().push(b.clone());
                axum::Json(b)
            }),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(|State(st): State<Live>, Query(q): Query<HashMap<String, String>>| async move {
                // Everything here is "upcoming"; "active" answers nothing.
                let upcoming = q.get("broadcastStatus").map(String::as_str) == Some("upcoming");
                let items = if upcoming { st.broadcasts.lock().unwrap().clone() } else { vec![] };
                axum::Json(json!({ "items": items }))
            })
            .post(|State(st): State<Live>, axum::Json(mut b): axum::Json<serde_json::Value>| async move {
                st.calls.lock().unwrap().push(format!("broadcasts.insert {}", b["status"]["privacyStatus"]));
                assert_eq!(b["contentDetails"]["enableAutoStart"], true);
                assert!(b["snippet"]["scheduledStartTime"].as_str().unwrap().ends_with('Z'));
                b["id"] = json!("bv");
                st.broadcasts.lock().unwrap().push(b.clone());
                axum::Json(b)
            }),
        )
        .route(
            "/api/liveBroadcasts/bind",
            axum::routing::post(|State(st): State<Live>, Query(q): Query<HashMap<String, String>>| async move {
                st.calls.lock().unwrap().push(format!("bind {} {}", q["id"], q["streamId"]));
                for b in st.broadcasts.lock().unwrap().iter_mut() {
                    if b["id"] == json!(q["id"]) {
                        b["contentDetails"]["boundStreamId"] = json!(q["streamId"]);
                    }
                }
                axum::Json(json!({ "id": q["id"] }))
            }),
        )
        .with_state(st);
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(rest, app).await });
    let ep = Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: "http://127.0.0.1:9".into(),
    };
    (ep, calls)
}

#[tokio::test]
async fn the_vertical_stream_is_made_once_and_never_taken_for_the_normal_one() {
    let (ep, calls) = fake_live().await;
    let api = video_api(ep);
    let v = api.vertical_ingest().await.unwrap();
    assert_eq!(v.key, "vertical-key");
    assert_eq!(
        calls.lock().unwrap().iter().filter(|c| !c.ends_with(".list")).cloned().collect::<Vec<_>>(),
        vec!["streams.insert", "broadcasts.insert \"unlisted\"", "bind bv sv"],
        "made once, with the normal live's privacy"
    );
    // Again: found, nothing new made.
    calls.lock().unwrap().clear();
    assert_eq!(api.vertical_ingest().await.unwrap().key, "vertical-key");
    assert!(calls.lock().unwrap().iter().all(|c| c.ends_with(".list")), "{:?}", calls.lock().unwrap());
    // The normal YouTube destination keeps its own key, even with the vertical broadcast upcoming.
    assert_eq!(api.ingest().await.unwrap().key, "main-key");
}

#[test]
fn utc_timestamps() {
    use crate::api::rfc3339_utc;
    assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_utc(1_793_097_000_000), "2026-10-27T10:30:00Z");
    assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z");
}

#[test]
fn a_big_thumbnail_gets_time_to_upload() {
    use crate::api::{MAX_THUMBNAIL_BYTES, thumbnail_deadline};
    assert_eq!(thumbnail_deadline(200 * 1024).as_secs(), 15);
    // 50 MB at about 2 Mbps, within the 5-minute cap.
    let big = thumbnail_deadline(MAX_THUMBNAIL_BYTES).as_secs();
    assert!((200..=300).contains(&big), "{big}");
    assert_eq!(thumbnail_deadline(usize::MAX / 2).as_secs(), 300);
}
