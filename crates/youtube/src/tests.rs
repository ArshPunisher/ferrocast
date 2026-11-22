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
    fakes_with_token_failures(0).await
}

/// As [`fakes`]; the token endpoint answers 503 the first `failures` times (Google having a bad
/// moment, or the Wi-Fi blipping just as the token was refreshed).
async fn fakes_with_token_failures(failures: usize) -> (Endpoints, Arc<Mutex<Script>>, Arc<Mutex<Vec<String>>>) {
    fakes_issuing(failures, "access-0").await
}

/// As [`fakes_with_token_failures`]; the token endpoint hands out `issued` (the chat only takes
/// "access-0").
async fn fakes_issuing(
    failures: usize,
    issued: &'static str,
) -> (Endpoints, Arc<Mutex<Script>>, Arc<Mutex<Vec<String>>>) {
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
                    let n = {
                        let mut f = f.lock().unwrap();
                        f.push(body);
                        f.len()
                    };
                    if n <= failures {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            axum::Json(json!({ "error": "backend" })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({ "access_token": issued, "refresh_token": "refresh-0", "expires_in": 3600 })),
                    )
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

/// One failed refresh of the sign-in (the token endpoint having a bad moment, or the Wi-Fi blipping
/// right then, or the Keychain refusing the save) is not the streamer signing out: the chat tries
/// again and comes back by itself. Only a refresh that Google refuses for good ends it.
#[tokio::test]
async fn one_failed_token_refresh_does_not_end_the_chat_for_good() {
    use ferrocast_core::event::ConnectorStatus as S;
    let (ep, script, forms) = fakes_with_token_failures(1).await;
    // The token the chat has is stale (the server wants access-0): it is refused, then refreshed.
    let tokens = Tokens { access_token: "stale".into(), refresh_token: "r".into(), expires_at: now_ms() + 3_600_000 };
    let api = Arc::new(Api::new(http_client(), ep, app(), tokens, Arc::new(|_| Ok(()))));
    let (sink, mut rx) = mpsc::channel(64);
    tokio::spawn(stream::run(api, sink, Arc::new(Notify::new()), Arc::default()));
    let mut saw_chat = false;
    loop {
        let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(12), rx.recv()).await else {
            panic!("the chat never came back (the connector statuses stopped)");
        };
        match e.kind {
            EventKind::Connector(S::SignInNeeded) => panic!("one failed refresh was taken for a sign-out"),
            EventKind::ChatMessage { .. } => saw_chat = true,
            EventKind::StreamOffline => break,
            _ => {}
        }
    }
    assert!(saw_chat, "the chat flowed after the refresh worked");
    assert_eq!(forms.lock().unwrap().len(), 2, "one refresh failed, the next worked");
    assert!(script.lock().unwrap().requests.len() >= 2);
}

/// A fresh token the chat still refuses means the sign-in no longer works (access taken away): it
/// says so and stops, after a couple of tries, instead of refreshing in a tight loop.
#[tokio::test]
async fn a_fresh_token_that_is_refused_again_ends_the_chat_asking_for_a_sign_in() {
    use ferrocast_core::event::ConnectorStatus as S;
    let (ep, _script, forms) = fakes_issuing(0, "access-never-accepted").await;
    let tokens = Tokens { access_token: "stale".into(), refresh_token: "r".into(), expires_at: now_ms() + 3_600_000 };
    let api = Arc::new(Api::new(http_client(), ep, app(), tokens, Arc::new(|_| Ok(()))));
    let (sink, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(stream::run(api, sink, Arc::new(Notify::new()), Arc::default()));
    loop {
        let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(8), rx.recv()).await else {
            panic!("it never said the sign-in is needed");
        };
        if matches!(e.kind, EventKind::Connector(S::SignInNeeded)) {
            break;
        }
    }
    assert!(forms.lock().unwrap().len() <= 3, "not a loop of refreshes: {}", forms.lock().unwrap().len());
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

/// The Keychain refusing to save the refreshed sign-in (locked, a hiccup) must not throw it away:
/// Twitch and Kick keep it for the session and save again later; YouTube refused the refresh and
/// the chat with it. The next refresh would have used the same old refresh token, and a provider
/// that rotates it would have signed the streamer out for good.
#[tokio::test]
async fn a_refreshed_sign_in_the_keychain_wouldnt_save_is_kept_for_the_session() {
    let (ep, _script, forms) = fakes().await;
    // Expired: the first use refreshes it.
    let tokens = Tokens { access_token: "old".into(), refresh_token: "r".into(), expires_at: 1 };
    let saves = Arc::new(Mutex::new(0usize));
    let counted = saves.clone();
    let on_tokens: crate::api::OnTokens = Arc::new(move |_| {
        let mut n = counted.lock().unwrap();
        *n += 1;
        if *n == 1 { Err("the Keychain is locked".to_string()) } else { Ok(()) }
    });
    let api = Api::new(http_client(), ep, app(), tokens, on_tokens);
    assert_eq!(api.access().await.expect("the refreshed token is used"), "access-0");
    assert_eq!(api.access().await.unwrap(), "access-0", "and kept: no second refresh");
    assert_eq!(forms.lock().unwrap().len(), 1, "refreshed once");
    // Tried again, but not at every call: a locked Keychain isn't asked under the lock each time.
    for _ in 0..10 {
        api.access().await.unwrap();
    }
    let n = *saves.lock().unwrap();
    assert!((2..=2).contains(&n), "saved once, retried once, then left alone for a while: {n}");
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
    let p4 = puts.clone();
    let p5 = puts.clone();
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
                let status = q.get("broadcastStatus").map(String::as_str);
                let items = match status {
                    Some("upcoming") if scheduled => json!([{ "id": "vid-1" }]),
                    // Two ended streams: yesterday's, and the one that just ended.
                    Some("completed") => json!([
                        { "id": "old", "snippet": { "actualStartTime": "2026-11-01T10:00:00Z" } },
                        { "id": "vid-1", "snippet": { "actualStartTime": "2026-11-02T01:00:00.250Z" } },
                    ]),
                    _ => json!([]),
                };
                axum::Json(json!({ "items": items }))
            })
            // With no broadcast, one is made (recorded like a PUT, as {"made": body}).
            .post(move |axum::Json(b): axum::Json<serde_json::Value>| {
                let p = p4.clone();
                async move {
                    p.lock().unwrap().push(json!({ "made": b }));
                    axum::Json(json!({ "id": "made-1" }))
                }
            }),
        )
        .route(
            "/api/liveBroadcasts/bind",
            axum::routing::post(move |Query(q): Query<HashMap<String, String>>| {
                let p = p5.clone();
                async move {
                    p.lock().unwrap().push(json!({ "bind": q.get("id"), "stream": q.get("streamId") }));
                    axum::Json(json!({ "id": q.get("id") }))
                }
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
async fn chapters_go_at_the_end_of_the_ended_streams_description() {
    let (ep, puts) = fake_videos(false).await;
    let api = video_api(ep);
    let start = crate::api::parse_rfc3339_ms("2026-11-02T01:00:00.250Z").unwrap();
    assert_eq!(start, 1_793_581_200_250);
    // Yesterday's is too early; this one went live a minute after the stream began here.
    assert_eq!(api.last_broadcast(start - 60_000).await.unwrap(), Some(("vid-1".into(), start)));
    assert_eq!(api.last_broadcast(start + 3_600_000).await.unwrap(), None);
    api.set_chapters("vid-1", "0:00 Start\n3:05 Gameplay\n10:00 Chat\n").await.unwrap();
    let body = puts.lock().unwrap()[0].clone();
    assert_eq!(body["snippet"]["description"], "Keep me\n\nChapters:\n0:00 Start\n3:05 Gameplay\n10:00 Chat");
    assert_eq!(body["snippet"]["title"], "Old");
    // Chapters put there before are replaced, the rest kept.
    let again = crate::api::with_chapters("Hi\n\nChapters:\n0:00 A\n1:00 B\n", "0:00 C\n");
    assert_eq!(again, "Hi\n\nChapters:\n0:00 C");
    assert_eq!(crate::api::with_chapters("", "0:00 C\n"), "Chapters:\n0:00 C");
    // Text after the old chapters (links, tags) stays, the new ones going last.
    assert_eq!(
        crate::api::with_chapters("Hi\n\nChapters:\n0:00 A\n1:02:03 B\n\nFollow me: x.com/me\n#gaming", "0:00 C\n"),
        "Hi\n\nFollow me: x.com/me\n#gaming\n\nChapters:\n0:00 C"
    );
    assert_eq!(
        crate::api::parse_rfc3339_ms("2026-11-02T06:30:00+05:30"),
        crate::api::parse_rfc3339_ms("2026-11-02T01:00:00Z")
    );
    assert_eq!(crate::api::parse_rfc3339_ms("yesterday"), None);
}

#[tokio::test]
async fn with_no_broadcast_one_is_made_for_the_stream_info() {
    use ferrocast_core::stream_info::StreamInfo;
    let (ep, puts) = fake_videos(false).await;
    let got =
        video_api(ep).update_broadcast_info(&StreamInfo { title: Some("Ranked".into()), ..Default::default() }).await;
    assert_eq!(got.unwrap(), vec!["title"]);
    let p = puts.lock().unwrap().clone();
    // Made public, starting and ending with the stream, bound to the channel's stream, then titled.
    assert_eq!(p[0]["made"]["status"]["privacyStatus"], "public");
    assert_eq!(p[0]["made"]["contentDetails"]["enableAutoStart"], true);
    assert_eq!(p[1], json!({ "bind": "made-1", "stream": "s1" }));
    assert_eq!((p[2]["id"].as_str(), p[2]["snippet"]["title"].as_str()), (Some("made-1"), Some("Ranked")));
}

#[test]
fn privacy_is_ordered_from_private_to_public_and_names_what_youtube_says() {
    use crate::Privacy;
    assert!(Privacy::Private < Privacy::Unlisted && Privacy::Unlisted < Privacy::Public);
    assert_eq!(Privacy::default(), Privacy::Public, "as before: public unless the streamer chose");
    for p in [Privacy::Public, Privacy::Unlisted, Privacy::Private] {
        assert_eq!(Privacy::parse(p.as_str()), Some(p));
    }
    for bad in ["", "Public", "members", "unlisted ", "PRIVATE"] {
        assert_eq!(Privacy::parse(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn a_broadcast_is_made_with_the_privacy_the_streamer_chose() {
    use crate::Privacy;
    use ferrocast_core::stream_info::StreamInfo;
    for (chosen, said) in [(Privacy::Public, "public"), (Privacy::Unlisted, "unlisted"), (Privacy::Private, "private")]
    {
        let (ep, puts) = fake_videos(false).await;
        let api = video_api(ep).with_privacy(Arc::new(move || chosen));
        api.update_broadcast_info(&StreamInfo { title: Some("Ranked".into()), ..Default::default() }).await.unwrap();
        let p = puts.lock().unwrap().clone();
        assert_eq!(p[0]["made"]["status"]["privacyStatus"], said);
        assert_eq!(p[0]["made"]["status"]["selfDeclaredMadeForKids"], false, "that part is not the choice");
    }
}

#[tokio::test]
async fn the_choice_is_read_when_a_broadcast_is_made_not_when_the_api_is_built() {
    use crate::Privacy;
    use ferrocast_core::stream_info::StreamInfo;
    let cell = Arc::new(std::sync::atomic::AtomicU8::new(2));
    let read = cell.clone();
    let source = Arc::new(move || match read.load(std::sync::atomic::Ordering::SeqCst) {
        0 => Privacy::Private,
        1 => Privacy::Unlisted,
        _ => Privacy::Public,
    });
    let (ep, puts) = fake_videos(false).await;
    let api = video_api(ep).with_privacy(source);
    // The streamer changes the setting after the Api exists, before the broadcast is made.
    cell.store(0, std::sync::atomic::Ordering::SeqCst);
    api.update_broadcast_info(&StreamInfo { title: Some("Ranked".into()), ..Default::default() }).await.unwrap();
    assert_eq!(puts.lock().unwrap()[0]["made"]["status"]["privacyStatus"], "private");
}

#[tokio::test]
async fn the_vertical_broadcast_never_shows_more_than_the_choice_or_the_normal_live() {
    use crate::Privacy;
    // The normal live is unlisted (fake_live). Public chosen: it copies unlisted, as before.
    // Private chosen: private, though the normal live is more public than that.
    for (chosen, said) in
        [(Privacy::Public, "unlisted"), (Privacy::Unlisted, "unlisted"), (Privacy::Private, "private")]
    {
        let (ep, calls) = fake_live().await;
        video_api(ep).with_privacy(Arc::new(move || chosen)).vertical_ingest().await.unwrap();
        let log = calls.lock().unwrap().clone();
        assert!(
            log.iter().any(|c| c.starts_with(&format!("broadcasts.insert \"{said}\""))),
            "{chosen:?} should make it {said}: {log:?}"
        );
    }
    // No normal live to copy: the choice itself.
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k-main",
        )]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    video_api(ep).with_privacy(Arc::new(|| Privacy::Unlisted)).vertical_ingest().await.unwrap();
    let log = calls.lock().unwrap().clone();
    assert!(log.iter().any(|c| c.contains("POST /liveBroadcasts ") && c.contains("privacy=unlisted")), "{log:?}");
}

#[tokio::test]
async fn going_live_with_no_broadcast_makes_one_with_the_choice_too() {
    use crate::Privacy;
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k-main",
        )]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    video_api(ep).with_privacy(Arc::new(|| Privacy::Private)).ingest().await.unwrap();
    let log = calls.lock().unwrap().clone();
    assert!(
        log.iter().any(|c| c.starts_with("POST /liveBroadcasts b-new-") && c.contains("privacy=private")),
        "{log:?}"
    );
}

#[tokio::test]
async fn one_broadcast_is_made_even_when_youtube_is_slow_to_list_it() {
    use ferrocast_core::stream_info::StreamInfo;
    // This fake never lists what it made (as YouTube right after an insert).
    let (ep, puts) = fake_videos(false).await;
    let api = video_api(ep);
    let info = StreamInfo { title: Some("A".into()), ..Default::default() };
    let (a, b, c) = tokio::join!(api.update_broadcast_info(&info), api.set_thumbnail(b"x", "image/png"), api.ingest());
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    let made = puts.lock().unwrap().iter().filter(|p| p.get("made").is_some()).count();
    assert_eq!(made, 1, "{:?}", puts.lock().unwrap());
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
    // No broadcast: one is made, and the thumbnail goes on it.
    let (ep, puts) = fake_videos(false).await;
    video_api(ep).set_thumbnail(b"x", "image/png").await.unwrap();
    let last = puts.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["thumbnail"], "made-1");
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

/// 19 Nov, live: the channel's key was an old one set to 1080p, so a 1440p stream went out at
/// 1080p. Ferrocast makes its own key (any resolution), binds the broadcast to it and sends
/// there; a key that takes any resolution is used as it is.
#[tokio::test]
async fn a_key_set_to_one_resolution_is_swapped_for_ferrocasts_own() {
    let (ep, calls) = fake_live_keyed("1080p").await;
    let i = video_api(ep).ingest().await.unwrap();
    assert_eq!(i.key, "vertical-key", "the key made now (the fake names every new one so)");
    let calls = calls.lock().unwrap().clone();
    assert!(calls.contains(&"streams.insert".to_string()), "{calls:?}");
    assert!(calls.contains(&"bind b1 sv".to_string()), "the broadcast moved to it: {calls:?}");

    let (ep, calls) = fake_live_keyed("variable").await;
    let i = video_api(ep).ingest().await.unwrap();
    assert_eq!(i.key, "main-key");
    assert!(!calls.lock().unwrap().iter().any(|c| c == "streams.insert" || c.starts_with("bind")));
}

#[tokio::test]
async fn ingest_asks_youtube_for_the_channels_stream() {
    let (ep, _) = fake_videos(false).await;
    let i = video_api(ep).ingest().await.unwrap();
    assert_eq!((i.url.as_str(), i.key.as_str()), ("rtmps://a.rtmps.youtube.com/live2", "abcd-efgh-ijkl"));
}

/// A YouTube that keeps the streams and broadcasts it's given (for the vertical stream).
async fn fake_live() -> (Endpoints, Arc<Mutex<Vec<String>>>) {
    fake_live_keyed("variable").await
}

/// [`fake_live`] with the channel's default key set to `resolution`.
async fn fake_live_keyed(resolution: &str) -> (Endpoints, Arc<Mutex<Vec<String>>>) {
    use axum::extract::{Query, State};
    use std::collections::HashMap;
    #[derive(Clone, Default)]
    struct Live {
        streams: Arc<Mutex<Vec<serde_json::Value>>>,
        broadcasts: Arc<Mutex<Vec<serde_json::Value>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }
    let st = Live::default();
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    st.streams.lock().unwrap().push(json!({ "id": "s1", "snippet": { "title": "Default", "isDefaultStream": true },
        "cdn": { "resolution": resolution, "ingestionInfo": { "streamName": "main-key", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" } } }));
    // Waiting for "Go live" in YouTube Studio (auto-start off), with a custom thumbnail.
    st.broadcasts.lock().unwrap().push(json!({ "id": "b1",
        "snippet": { "title": "Day 41 ranked", "description": "Ranked all night",
            "thumbnails": { "high": { "url": format!("{base}/img/b1.jpg") } } },
        "status": { "privacyStatus": "unlisted", "lifeCycleStatus": "ready" },
        "contentDetails": { "boundStreamId": "s1", "enableAutoStart": false,
            "monitorStream": { "enableMonitorStream": true, "broadcastStreamDelayMs": 0 } } }));
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
            .put(|State(st): State<Live>, axum::Json(b): axum::Json<serde_json::Value>| async move {
                // Recorded with the monitor setting, so a change to it fails the test.
                st.calls.lock().unwrap().push(format!(
                    "broadcasts.update {} autostart={} bound={} monitor={}",
                    b["id"].as_str().unwrap_or(""),
                    b["contentDetails"]["enableAutoStart"],
                    b["contentDetails"].get("boundStreamId").is_some(),
                    b["contentDetails"]["monitorStream"]["enableMonitorStream"]
                ));
                axum::Json(b)
            })
            .post(|State(st): State<Live>, axum::Json(mut b): axum::Json<serde_json::Value>| async move {
                st.calls.lock().unwrap().push(format!("broadcasts.insert {} {}", b["status"]["privacyStatus"], b["snippet"]["description"]));
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
        .route("/img/b1.jpg", axum::routing::get(|| async { ([("content-type", "image/jpeg")], vec![0xFFu8, 0xD8, 0xFF, 0xE0]) }))
        .route(
            "/api/upload/thumbnails/set",
            axum::routing::post(|State(st): State<Live>, Query(q): Query<HashMap<String, String>>, body: axum::body::Bytes| async move {
                st.calls.lock().unwrap().push(format!("thumbnail {} {}", q["videoId"], body.len()));
                axum::Json(json!({ "items": [] }))
            }),
        )
        .with_state(st);
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
        vec!["streams.insert", "broadcasts.insert \"unlisted\" \"Ranked all night\"", "bind bv sv", "thumbnail bv 4"],
        "made once, with the normal live's privacy, description and thumbnail"
    );
    // Again: found, nothing new made.
    calls.lock().unwrap().clear();
    assert_eq!(api.vertical_ingest().await.unwrap().key, "vertical-key");
    assert!(calls.lock().unwrap().iter().all(|c| c.ends_with(".list")), "{:?}", calls.lock().unwrap());
    // The normal YouTube destination keeps its own key, even with the vertical broadcast upcoming,
    // and its broadcast, waiting for "Go live" in YouTube Studio, now starts with the stream.
    calls.lock().unwrap().clear();
    assert_eq!(api.ingest().await.unwrap().key, "main-key");
    assert!(
        calls.lock().unwrap().contains(&"broadcasts.update b1 autostart=true bound=false monitor=true".to_string()),
        "{:?}",
        calls.lock().unwrap()
    );
    // Stream info goes to both broadcasts: the normal one and the vertical one.
    calls.lock().unwrap().clear();
    api.set_thumbnail(b"\x89PNG", "image/png").await.unwrap();
    let thumbs: Vec<String> = calls.lock().unwrap().iter().filter(|c| c.starts_with("thumbnail")).cloned().collect();
    assert_eq!(thumbs, ["thumbnail b1 4", "thumbnail bv 4"]);
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

// ---------------------------------------------------------------- M10-01 YouTube safety & parity

#[derive(Clone, Default)]
struct ScriptedYoutube {
    stream_pages: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    broadcast_pages: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    /// When true, newly inserted streams/broadcasts are NOT added to GET lists (simulates list lag).
    lag_lists: bool,
    /// Optional HTTP status + body for POST /liveBroadcasts.
    fail_broadcast_insert: Option<(u16, serde_json::Value)>,
    /// Optional HTTP status + body for POST /liveBroadcasts/bind.
    fail_broadcast_bind: Option<(u16, serde_json::Value)>,
    /// When true, page 1 returns a repeating nextPageToken to test cycle protection.
    repeat_token: bool,
    calls: Arc<Mutex<Vec<String>>>,
    next_id: Arc<std::sync::atomic::AtomicUsize>,
}

async fn start_scripted(st: ScriptedYoutube) -> (Endpoints, Arc<Mutex<Vec<String>>>) {
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    let calls = st.calls.clone();
    let app = axum::Router::new()
        .route(
            "/api/liveStreams",
            axum::routing::get(|State(st): State<ScriptedYoutube>, Query(q): Query<HashMap<String, String>>| async move {
                let page_idx: usize = q.get("pageToken").and_then(|t| t.strip_prefix("sp-")).and_then(|n| n.parse().ok()).unwrap_or(0);
                st.calls.lock().unwrap().push(format!("GET /liveStreams page={page_idx}"));
                let pages = st.stream_pages.lock().unwrap();
                let items = pages.get(page_idx).cloned().unwrap_or_default();
                let mut resp = json!({ "items": items });
                if st.repeat_token {
                    resp["nextPageToken"] = json!("sp-0");
                } else if page_idx + 1 < pages.len() {
                    resp["nextPageToken"] = json!(format!("sp-{}", page_idx + 1));
                }
                axum::Json(resp)
            })
            .post(|State(st): State<ScriptedYoutube>, axum::Json(mut b): axum::Json<serde_json::Value>| async move {
                let n = st.next_id.fetch_add(1, Ordering::SeqCst) + 1;
                let id = format!("s-new-{n}");
                st.calls.lock().unwrap().push(format!("POST /liveStreams {id}"));
                b["id"] = json!(id);
                b["cdn"]["ingestionInfo"] = json!({
                    "streamName": format!("var-key-{n}"),
                    "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2"
                });
                if !st.lag_lists {
                    let mut pages = st.stream_pages.lock().unwrap();
                    if pages.is_empty() {
                        pages.push(vec![]);
                    }
                    pages[0].push(b.clone());
                }
                axum::Json(b)
            }),
        )
        .route(
            "/api/liveBroadcasts",
            axum::routing::get(|State(st): State<ScriptedYoutube>, Query(q): Query<HashMap<String, String>>| async move {
                let status = q.get("broadcastStatus").cloned().unwrap_or_default();
                let page_idx: usize = q.get("pageToken").and_then(|t| t.strip_prefix("bp-")).and_then(|n| n.parse().ok()).unwrap_or(0);
                st.calls.lock().unwrap().push(format!("GET /liveBroadcasts status={status} page={page_idx}"));
                if status != "upcoming" {
                    return axum::Json(json!({ "items": [] }));
                }
                let pages = st.broadcast_pages.lock().unwrap();
                let items = pages.get(page_idx).cloned().unwrap_or_default();
                let mut resp = json!({ "items": items });
                if st.repeat_token {
                    resp["nextPageToken"] = json!("bp-0");
                } else if page_idx + 1 < pages.len() {
                    resp["nextPageToken"] = json!(format!("bp-{}", page_idx + 1));
                }
                axum::Json(resp)
            })
            .post(|State(st): State<ScriptedYoutube>, axum::Json(mut b): axum::Json<serde_json::Value>| async move {
                if let Some((code, body)) = &st.fail_broadcast_insert {
                    st.calls.lock().unwrap().push(format!("POST /liveBroadcasts err={code}"));
                    return (StatusCode::from_u16(*code).unwrap(), axum::Json(body.clone()));
                }
                let n = st.next_id.fetch_add(1, Ordering::SeqCst) + 1;
                let id = format!("b-new-{n}");
                let title = b.pointer("/snippet/title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let privacy = b.pointer("/status/privacyStatus").and_then(|v| v.as_str()).unwrap_or("").to_string();
                st.calls.lock().unwrap().push(format!("POST /liveBroadcasts {id} privacy={privacy} title={title}"));
                b["id"] = json!(id);
                if !st.lag_lists {
                    let mut pages = st.broadcast_pages.lock().unwrap();
                    if pages.is_empty() {
                        pages.push(vec![]);
                    }
                    pages[0].push(b.clone());
                }
                (StatusCode::OK, axum::Json(b))
            })
            .put(|State(st): State<ScriptedYoutube>, axum::Json(b): axum::Json<serde_json::Value>| async move {
                let id = b["id"].as_str().unwrap_or("").to_string();
                st.calls.lock().unwrap().push(format!("PUT /liveBroadcasts {id}"));
                axum::Json(b)
            }),
        )
        .route(
            "/api/liveBroadcasts/bind",
            axum::routing::post(|State(st): State<ScriptedYoutube>, Query(q): Query<HashMap<String, String>>| async move {
                let bid = q.get("id").cloned().unwrap_or_default();
                let sid = q.get("streamId").cloned().unwrap_or_default();
                if let Some((code, body)) = &st.fail_broadcast_bind {
                    st.calls.lock().unwrap().push(format!("POST /liveBroadcasts/bind {bid}->{sid} err={code}"));
                    return (StatusCode::from_u16(*code).unwrap(), axum::Json(body.clone()));
                }
                st.calls.lock().unwrap().push(format!("POST /liveBroadcasts/bind {bid}->{sid}"));
                for page in st.broadcast_pages.lock().unwrap().iter_mut() {
                    for b in page.iter_mut() {
                        if b["id"] == json!(bid) {
                            b["contentDetails"]["boundStreamId"] = json!(sid);
                        }
                    }
                }
                (StatusCode::OK, axum::Json(json!({ "id": bid })))
            }),
        )
        .route(
            "/api/videos",
            axum::routing::get(|State(st): State<ScriptedYoutube>, Query(q): Query<HashMap<String, String>>| async move {
                let id = q.get("id").cloned().unwrap_or_else(|| "v1".into());
                st.calls.lock().unwrap().push(format!("GET /videos {id}"));
                axum::Json(json!({ "items": [{ "id": id, "snippet": { "title": "Old", "description": "", "categoryId": "20" } }] }))
            })
            .put(|State(st): State<ScriptedYoutube>, axum::Json(b): axum::Json<serde_json::Value>| async move {
                let id = b["id"].as_str().unwrap_or("").to_string();
                st.calls.lock().unwrap().push(format!("PUT /videos {id}"));
                axum::Json(b)
            }),
        )
        .route(
            "/api/upload/thumbnails/set",
            axum::routing::post(|State(st): State<ScriptedYoutube>, Query(q): Query<HashMap<String, String>>| async move {
                st.calls.lock().unwrap().push(format!("POST thumbnails.set {}", q.get("videoId").cloned().unwrap_or_default()));
                axum::Json(json!({ "items": [] }))
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

fn stream_item(
    id: &str,
    title: &str,
    default: bool,
    res: &str,
    fps: &str,
    ingest_type: &str,
    key: &str,
) -> serde_json::Value {
    let info = if ingest_type == "rtmp" {
        json!({ "streamName": key, "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" })
    } else {
        json!({ "ingestionAddress": "https://a.upload.youtube.com/http_upload_hls?cid=x" })
    };
    json!({
        "id": id,
        "snippet": { "title": title, "isDefaultStream": default },
        "cdn": { "resolution": res, "frameRate": fps, "ingestionType": ingest_type, "ingestionInfo": info }
    })
}

fn has_write_call(calls: &[String]) -> bool {
    calls.iter().any(|c| c.starts_with("POST ") || c.starts_with("PUT "))
}

#[tokio::test]
async fn peek_ingest_makes_zero_writes_even_with_no_broadcast_or_fixed_key() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "1080p", "30fps", "rtmp", "k1",
        )]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let api = video_api(ep);
    assert_eq!(api.peek_ingest().await.unwrap(), "rtmps://a.rtmps.youtube.com/live2");
    assert_eq!(api.peek_vertical_ingest().await.unwrap(), "rtmps://a.rtmps.youtube.com/live2");
    let got = calls.lock().unwrap().clone();
    assert!(!has_write_call(&got), "peek must never POST/PUT: {got:?}");
}

#[tokio::test]
async fn stream_info_edit_then_go_live_within_10_min_still_swaps_a_fixed_key() {
    use ferrocast_core::stream_info::StreamInfo;
    // Case 1: fixed 1080p key, no broadcast yet, normal list visibility.
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1",
            "Default",
            true,
            "1080p",
            "variable",
            "rtmp",
            "old-1080p",
        )]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let api = video_api(ep);
    api.update_broadcast_info(&StreamInfo { title: Some("Ranked".into()), ..Default::default() }).await.unwrap();
    let ing = api.ingest().await.unwrap();
    assert!(ing.key.starts_with("var-key-"), "must use Ferrocast's variable key, not old-1080p: {}", ing.key);
    let log = calls.lock().unwrap().clone();
    assert_eq!(log.iter().filter(|c| c.starts_with("POST /liveBroadcasts ")).count(), 1, "{log:?}");
    assert_eq!(log.iter().filter(|c| c.starts_with("POST /liveStreams ")).count(), 1, "{log:?}");

    // Case 2: YouTube's lists lag (newly created broadcast isn't listed yet) AND the channel's key
    // turns fixed between edit and Go Live — `just_made` must never skip replacing a bad key.
    let stream_pages =
        Arc::new(Mutex::new(vec![vec![stream_item("s1", "Default", true, "variable", "variable", "rtmp", "k-var")]]));
    let st2 = ScriptedYoutube { stream_pages: stream_pages.clone(), lag_lists: true, ..Default::default() };
    let (ep2, calls2) = start_scripted(st2).await;
    let api2 = video_api(ep2);
    api2.update_broadcast_info(&StreamInfo { title: Some("Before".into()), ..Default::default() }).await.unwrap();
    // Key becomes fixed 1080p before Go Live (within 10 min, broadcast still unlisted).
    stream_pages.lock().unwrap()[0][0]["cdn"]["resolution"] = json!("1080p");
    let ing2 = api2.ingest().await.unwrap();
    assert!(ing2.key.starts_with("var-key-"), "just_made must not suppress swap: {}", ing2.key);
    let log2 = calls2.lock().unwrap().clone();
    assert!(
        log2.iter().any(|c| c.starts_with("POST /liveBroadcasts/bind b-new-1->s-new-")),
        "must rebind just_made broadcast: {log2:?}"
    );
    assert_eq!(
        log2.iter().filter(|c| c.starts_with("POST /liveBroadcasts ")).count(),
        1,
        "no duplicate broadcast: {log2:?}"
    );
}

#[tokio::test]
async fn a_30fps_or_non_rtmp_key_gets_swapped_for_ferrocasts_variable_rtmp_key() {
    // 30 fps fixed key with variable resolution.
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "30fps", "rtmp", "k-30fps",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![json!({
            "id": "b1",
            "snippet": { "title": "Today", "scheduledStartTime": crate::api::rfc3339_utc(now_ms() + 10 * 60_000) },
            "status": { "lifeCycleStatus": "ready" },
            "contentDetails": { "boundStreamId": "s1", "enableAutoStart": true }
        })]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let ing = video_api(ep).ingest().await.unwrap();
    assert!(ing.key.starts_with("var-key-"), "30fps key must be swapped: {}", ing.key);
    let log = calls.lock().unwrap().clone();
    assert!(log.iter().any(|c| c.starts_with("POST /liveStreams ")), "{log:?}");
    assert!(log.iter().any(|c| c.starts_with("POST /liveBroadcasts/bind b1->s-new-")), "{log:?}");

    // Non-RTMP (HLS) key with variable resolution & variable fps.
    let st_hls = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s-hls", "HLS", true, "variable", "variable", "hls", "",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![json!({
            "id": "b2",
            "snippet": { "title": "Today" },
            "status": { "lifeCycleStatus": "ready" },
            "contentDetails": { "boundStreamId": "s-hls", "enableAutoStart": true }
        })]])),
        ..Default::default()
    };
    let (ep_hls, calls_hls) = start_scripted(st_hls).await;
    let ing_hls = video_api(ep_hls).ingest().await.unwrap();
    assert!(ing_hls.key.starts_with("var-key-"), "HLS key must be swapped: {}", ing_hls.key);
    let log_hls = calls_hls.lock().unwrap().clone();
    assert!(log_hls.iter().any(|c| c.starts_with("POST /liveBroadcasts/bind b2->s-new-")), "{log_hls:?}");
}

#[tokio::test]
async fn a_next_week_broadcast_is_untouched_unless_explicitly_picked() {
    use ferrocast_core::stream_info::StreamInfo;
    let next_week = crate::api::rfc3339_utc(now_ms() + 7 * 86_400_000);
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k-main",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![json!({
            "id": "b-next-week",
            "snippet": { "title": "Tournament Finals", "description": "Next week!", "scheduledStartTime": next_week },
            "status": { "privacyStatus": "unlisted", "lifeCycleStatus": "ready" },
            "contentDetails": { "boundStreamId": "s1", "enableAutoStart": false }
        })]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let api = video_api(ep);
    // Stream info edit must not touch next week's broadcast.
    api.update_broadcast_info(&StreamInfo { title: Some("Quick stream".into()), ..Default::default() }).await.unwrap();
    // Going live must not rebind or enable autoStart on next week's broadcast.
    let ing = api.ingest().await.unwrap();
    assert_eq!(ing.key, "k-main");
    let log = calls.lock().unwrap().clone();
    assert!(!log.iter().any(|c| c.contains("b-next-week")), "next-week broadcast must be untouched: {log:?}");
    assert!(log.iter().any(|c| c.starts_with("POST /liveBroadcasts b-new-")), "a fresh broadcast was made: {log:?}");

    // Explicitly picking next week's broadcast via ingest_for(Some(...)) DOES target it.
    calls.lock().unwrap().clear();
    let ing_picked = api.ingest_for(Some("b-next-week")).await.unwrap();
    assert_eq!(ing_picked.key, "k-main");
    let picked_log = calls.lock().unwrap().clone();
    assert!(picked_log.contains(&"PUT /liveBroadcasts b-next-week".to_string()), "{picked_log:?}");

    // Vertical ingest also ignores a next-week broadcast (does not copy its title or privacy).
    let st_v = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k-main",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![json!({
            "id": "b-next-week",
            "snippet": { "title": "Tournament Finals", "scheduledStartTime": crate::api::rfc3339_utc(now_ms() + 7 * 86_400_000) },
            "status": { "privacyStatus": "unlisted", "lifeCycleStatus": "ready" },
            "contentDetails": { "boundStreamId": "s1", "enableAutoStart": false }
        })]])),
        ..Default::default()
    };
    let (ep_v, calls_v) = start_scripted(st_v).await;
    video_api(ep_v).vertical_ingest().await.unwrap();
    let vlog = calls_v.lock().unwrap().clone();
    assert!(
        vlog.iter().any(|c| c.contains("POST /liveBroadcasts ")
            && c.contains("title=Live now")
            && c.contains("privacy=public")),
        "vertical must not copy next-week event: {vlog:?}"
    );
}

#[tokio::test]
async fn forbidden_on_make_broadcast_or_bind_returns_err_instead_of_streaming_into_nothing() {
    // 403 quotaExceeded on make_broadcast.
    let st_quota = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k1",
        )]])),
        fail_broadcast_insert: Some((
            403,
            json!({ "error": { "message": "Quota exceeded.", "errors": [{ "reason": "quotaExceeded" }] } }),
        )),
        ..Default::default()
    };
    let (ep, _) = start_scripted(st_quota).await;
    assert_eq!(video_api(ep).ingest().await.unwrap_err(), YoutubeError::QuotaExceeded);

    // 403 forbidden with multi-line reason on make_broadcast → single-line Err.
    let st_403 = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k1",
        )]])),
        fail_broadcast_insert: Some((
            403,
            json!({ "error": { "message": "Live streaming\nis not enabled.", "errors": [{ "reason": "liveStreamingNotEnabled" }] } }),
        )),
        ..Default::default()
    };
    let (ep2, _) = start_scripted(st_403).await;
    let err = video_api(ep2).ingest().await.unwrap_err().to_string();
    assert!(!err.contains('\n'), "error must be one line: {err:?}");
    assert!(err.contains("Live streaming is not enabled"), "{err}");

    // 403 on bind → Err (never stream into an unbound key).
    let st_bind = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k1",
        )]])),
        fail_broadcast_bind: Some((
            403,
            json!({ "error": { "message": "Cannot bind stream.", "errors": [{ "reason": "forbidden" }] } }),
        )),
        ..Default::default()
    };
    let (ep3, _) = start_scripted(st_bind).await;
    let err3 = video_api(ep3).ingest().await.unwrap_err().to_string();
    assert!(err3.contains("bind") && !err3.contains('\n'), "{err3}");
}

#[tokio::test]
async fn pagination_finds_streams_and_broadcasts_on_later_pages_and_guards_vertical_duplicates() {
    let next_week = crate::api::rfc3339_utc(now_ms() + 7 * 86_400_000);
    let soon = crate::api::rfc3339_utc(now_ms() + 15 * 60_000);
    // Page 1 has a fixed stream + next-week broadcast; page 2 has Ferrocast's own variable stream
    // and today's near-now broadcast.
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![
            vec![stream_item("s-old", "Default", true, "1080p", "30fps", "rtmp", "k-old")],
            vec![stream_item("s-fc", crate::api::MAIN_STREAM_TITLE, false, "variable", "variable", "rtmp", "k-fc-p2")],
        ])),
        broadcast_pages: Arc::new(Mutex::new(vec![
            vec![json!({
                "id": "b-week",
                "snippet": { "title": "Next week", "scheduledStartTime": next_week },
                "status": { "lifeCycleStatus": "ready" },
                "contentDetails": { "boundStreamId": "s-old", "enableAutoStart": false }
            })],
            vec![json!({
                "id": "b-soon",
                "snippet": { "title": "Today", "scheduledStartTime": soon },
                "status": { "lifeCycleStatus": "ready" },
                "contentDetails": { "boundStreamId": "s-old", "enableAutoStart": false }
            })],
        ])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let api = video_api(ep);
    let ing = api.ingest().await.unwrap();
    assert_eq!(ing.key, "k-fc-p2", "reused Ferrocast stream from page 2 without inserting");
    let log = calls.lock().unwrap().clone();
    assert!(!log.iter().any(|c| c.starts_with("POST /liveStreams")), "no duplicate stream: {log:?}");
    assert!(log.contains(&"POST /liveBroadcasts/bind b-soon->s-fc".to_string()), "{log:?}");
    assert!(log.contains(&"PUT /liveBroadcasts b-soon".to_string()), "{log:?}");
    assert!(!log.iter().any(|c| c.contains("b-week")), "next-week broadcast untouched: {log:?}");

    // Repeating pageToken must not infinite-loop.
    let st_loop = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k1",
        )]])),
        repeat_token: true,
        ..Default::default()
    };
    let (ep_loop, _) = start_scripted(st_loop).await;
    assert_eq!(video_api(ep_loop).peek_ingest().await.unwrap(), "rtmps://a.rtmps.youtube.com/live2");

    // Concurrent vertical_ingest() with lagging lists creates only ONE vertical stream and ONE
    // vertical broadcast (no duplicates).
    let st_vert = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k1",
        )]])),
        lag_lists: true,
        ..Default::default()
    };
    let (ep_vert, calls_vert) = start_scripted(st_vert).await;
    let api_vert = video_api(ep_vert);
    let (v1, v2) = tokio::join!(api_vert.vertical_ingest(), api_vert.vertical_ingest());
    assert_eq!(v1.unwrap().key, v2.unwrap().key);
    let vlog = calls_vert.lock().unwrap().clone();
    assert_eq!(vlog.iter().filter(|c| c.starts_with("POST /liveStreams ")).count(), 1, "{vlog:?}");
    assert_eq!(vlog.iter().filter(|c| c.starts_with("POST /liveBroadcasts ")).count(), 1, "{vlog:?}");

    // Existing vertical broadcast with autoStart: false gets auto-start enabled; existing unbound
    // main broadcast gets bound instead of streaming into nothing.
    let st_auto = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![
            stream_item("s1", "Default", true, "variable", "variable", "rtmp", "k1"),
            stream_item("sv1", crate::api::VERTICAL_STREAM_TITLE, false, "variable", "variable", "rtmp", "kv1"),
        ]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![
            json!({
                "id": "b-unbound",
                "snippet": { "title": "Unbound" },
                "status": { "lifeCycleStatus": "ready" },
                "contentDetails": { "enableAutoStart": true }
            }),
            json!({
                "id": "bv-manual",
                "snippet": { "title": "Shorts" },
                "status": { "lifeCycleStatus": "ready" },
                "contentDetails": { "boundStreamId": "sv1", "enableAutoStart": false }
            }),
        ]])),
        ..Default::default()
    };
    let (ep_auto, calls_auto) = start_scripted(st_auto).await;
    let api_auto = video_api(ep_auto);
    assert_eq!(api_auto.ingest().await.unwrap().key, "k1");
    assert_eq!(api_auto.vertical_ingest().await.unwrap().key, "kv1");
    let alog = calls_auto.lock().unwrap().clone();
    assert!(alog.contains(&"POST /liveBroadcasts/bind b-unbound->s1".to_string()), "{alog:?}");
    assert!(alog.contains(&"PUT /liveBroadcasts bv-manual".to_string()), "{alog:?}");
}

// --- limits: a burst is not the day's quota (M10-06) ------------------------------------

/// A REST API that answers each request from a script, in order, then with an empty list.
async fn rest_that_answers(script: Vec<(u16, serde_json::Value)>) -> (Endpoints, Arc<Mutex<usize>>) {
    use std::collections::VecDeque;
    let queue = Arc::new(Mutex::new(VecDeque::from(script)));
    let count = Arc::new(Mutex::new(0usize));
    let (q, n) = (queue.clone(), count.clone());
    let app = axum::Router::new().fallback(move || {
        let (q, n) = (q.clone(), n.clone());
        async move {
            *n.lock().unwrap() += 1;
            let next = q.lock().unwrap().pop_front();
            let (code, body) = next.unwrap_or((200, json!({ "items": [] })));
            let mut res = axum::response::Response::new(axum::body::Body::from(body.to_string()));
            *res.status_mut() = axum::http::StatusCode::from_u16(code).unwrap();
            res.headers_mut().insert("content-type", "application/json".parse().unwrap());
            if code == 429 {
                res.headers_mut().insert("retry-after", "120".parse().unwrap());
            }
            res
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    let ep = Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: "http://127.0.0.1:9".into(),
    };
    (ep, count)
}

fn google_error(reason: &str) -> serde_json::Value {
    json!({ "error": { "code": 403, "message": "No.", "errors": [{ "reason": reason }] } })
}

#[tokio::test]
async fn a_burst_limit_and_the_days_quota_are_told_apart() {
    for (script, want) in [
        ((403, google_error("rateLimitExceeded")), YoutubeError::RateLimited(Duration::from_secs(5))),
        ((403, google_error("userRateLimitExceeded")), YoutubeError::RateLimited(Duration::from_secs(5))),
        ((403, google_error("quotaExceeded")), YoutubeError::QuotaExceeded),
        ((403, google_error("dailyLimitExceeded")), YoutubeError::QuotaExceeded),
        // A 429 says how long (120 s): never more than a minute.
        ((429, json!({})), YoutubeError::RateLimited(Duration::from_secs(60))),
    ] {
        let (ep, _) = rest_that_answers(vec![script.clone()]).await;
        let got = video_api(ep).active_live_chat().await.unwrap_err();
        assert_eq!(got, want, "{script:?}");
    }
    // What the rest of the app hears: seconds for a burst, until Pacific midnight for the day.
    let burst: ferrocast_connector::ConnectorError = YoutubeError::RateLimited(Duration::from_secs(7)).into();
    assert!(
        matches!(burst, ferrocast_connector::ConnectorError::RateLimited { retry_after, .. } if retry_after == Duration::from_secs(7))
    );
    let day: ferrocast_connector::ConnectorError = YoutubeError::QuotaExceeded.into();
    let ferrocast_connector::ConnectorError::RateLimited { retry_after, .. } = day else { panic!("{day:?}") };
    assert!(retry_after != Duration::from_secs(3600), "not a blind hour");
    assert!(retry_after <= Duration::from_secs(86_400 + 300), "{retry_after:?}");
}

#[test]
fn the_quota_is_back_at_midnight_pacific_time() {
    let at = |s: &str| api::parse_rfc3339_ms(s).unwrap();
    let hours = |now: &str| quota_resets_in(at(now), 0).as_secs_f64() / 3600.0;
    // Winter (PST, UTC-8): noon in Los Angeles is 20:00 UTC, midnight 12 hours on.
    assert_eq!(hours("2026-11-20T20:00:00Z"), 12.0);
    // Summer (PDT, UTC-7).
    assert_eq!(hours("2026-07-01T12:00:00Z"), 19.0);
    // The night the clocks go forward (second Sunday of March, 2:00): 23:00 PST, an hour to midnight.
    assert_eq!(hours("2026-03-08T07:00:00Z"), 1.0);
    // The morning after it, in PDT.
    assert_eq!(hours("2026-03-08T12:00:00Z"), 19.0);
    // And back (first Sunday of November): still PDT on the Saturday, PST by Sunday noon.
    assert_eq!(hours("2026-10-31T20:00:00Z"), 11.0);
    assert_eq!(hours("2026-11-01T12:00:00Z"), 20.0);
    // The spread is added.
    assert_eq!(quota_resets_in(at("2026-11-20T20:00:00Z"), 90).as_secs(), 12 * 3600 + 90);
}

/// The statuses a run said, until `n` of them or the time is up.
async fn statuses(
    api: Arc<crate::api::Api>,
    waits: stream::Waits,
    n: usize,
) -> (Vec<ferrocast_core::event::ConnectorStatus>, Duration) {
    let (sink, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(stream::run_with(api, sink, Arc::new(Notify::new()), Arc::default(), waits));
    let t0 = std::time::Instant::now();
    let mut got = Vec::new();
    while got.len() < n {
        let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await else { break };
        if let EventKind::Connector(s) = e.kind {
            got.push(s);
        }
    }
    task.abort();
    (got, t0.elapsed())
}

#[tokio::test]
async fn a_burst_limit_retries_in_seconds_and_never_says_connected_between_failures() {
    use ferrocast_core::event::ConnectorStatus as S;
    let limited = || (403, google_error("rateLimitExceeded"));
    let (ep, _) = rest_that_answers(vec![limited(), limited(), limited()]).await;
    let waits = stream::Waits { rate: Some(Duration::from_millis(40)), quota: None };
    let (got, took) = statuses(Arc::new(video_api(ep)), waits, 5).await;
    assert_eq!(
        got,
        vec![
            S::Connected,
            S::Reconnecting { attempt: 1 },
            S::Reconnecting { attempt: 2 },
            S::Reconnecting { attempt: 3 },
            S::Connected,
        ],
        "one Connected to start, none between the failures, one when it worked"
    );
    assert!(took < Duration::from_secs(3), "a burst limit isn't an hour: {took:?}");
}

#[tokio::test]
async fn the_days_quota_pauses_and_says_so_without_connected_between_attempts() {
    use ferrocast_core::event::ConnectorStatus as S;
    let gone = || (403, google_error("quotaExceeded"));
    let (ep, calls) = rest_that_answers(vec![gone(), gone()]).await;
    let waits = stream::Waits { rate: None, quota: Some(Duration::from_millis(40)) };
    let (got, _) = statuses(Arc::new(video_api(ep)), waits, 4).await;
    assert!(
        matches!(&got[..], [S::Connected, S::Disconnected { .. }, S::Disconnected { .. }, S::Connected]),
        "{got:?}"
    );
    assert_eq!(*calls.lock().unwrap(), 3, "asked three times: it didn't poll while it waited");
}

#[test]
fn a_burst_backs_off_from_five_seconds_to_a_minute() {
    let s = Duration::from_secs;
    assert_eq!(stream::rate_backoff(1, s(0)), s(5));
    assert_eq!(stream::rate_backoff(2, s(0)), s(10));
    assert_eq!(stream::rate_backoff(5, s(0)), s(60));
    assert_eq!(stream::rate_backoff(9, s(0)), s(60));
    assert_eq!(stream::rate_backoff(1, s(30)), s(30), "never less than YouTube asked");
    assert_eq!(stream::rate_backoff(1, s(300)), s(60));
}

// --- quota units (M10-06) -----------------------------------------------------------

/// YouTube's units for each call in a fake's log: a read is 1, every write 50.
fn units(calls: &[String]) -> u32 {
    calls.iter().map(|c| if c.starts_with("GET ") { 1 } else { 50 }).sum()
}

fn reads(calls: &[String], what: &str) -> usize {
    calls.iter().filter(|c| c.starts_with(&format!("GET {what}"))).count()
}

/// A channel with its default key and a Ferrocast broadcast waiting on it, as after a first stream.
fn a_channel_with_a_broadcast() -> ScriptedYoutube {
    ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s1", "Default", true, "variable", "variable", "rtmp", "k-main",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![json!({
            "id": "b1",
            "snippet": { "title": "Live now" },
            "status": { "lifeCycleStatus": "ready" },
            "contentDetails": { "boundStreamId": "s1", "enableAutoStart": true }
        })]])),
        ..Default::default()
    }
}

/// A title and then a thumbnail, set one after the other as Go live does, listed the channel's
/// streams and broadcasts twice (3 units a time, each time); the lookup is kept for a minute and
/// forgotten at any write, and going live reads the channel as it is.
#[tokio::test]
async fn a_title_and_a_thumbnail_are_one_lookup_not_two() {
    use ferrocast_core::stream_info::StreamInfo;
    let info = StreamInfo { title: Some("Ranked all night".into()), ..Default::default() };

    // As before (nothing kept): two lookups.
    let (ep, calls) = start_scripted(a_channel_with_a_broadcast()).await;
    let old = video_api(ep).with_read_cache(Duration::ZERO);
    old.update_broadcast_info(&info).await.unwrap();
    old.set_thumbnail(b"x", "image/png").await.unwrap();
    let before = calls.lock().unwrap().clone();
    assert_eq!((reads(&before, "/liveStreams"), reads(&before, "/liveBroadcasts")), (2, 4), "{before:?}");

    // Now: one.
    let (ep, calls) = start_scripted(a_channel_with_a_broadcast()).await;
    let api = video_api(ep);
    api.update_broadcast_info(&info).await.unwrap();
    api.set_thumbnail(b"x", "image/png").await.unwrap();
    let after = calls.lock().unwrap().clone();
    assert_eq!((reads(&after, "/liveStreams"), reads(&after, "/liveBroadcasts")), (1, 2), "{after:?}");
    eprintln!("units for a title and a thumbnail: {} before, {} after", units(&before), units(&after));
    assert!(units(&after) < units(&before));

    // Going live reads the channel as it is now, never what was kept; and writes it makes empty it.
    let n = reads(&after, "/liveStreams");
    api.ingest().await.unwrap();
    let live = calls.lock().unwrap().clone();
    assert_eq!(reads(&live, "/liveStreams"), n + 1, "going live listed the streams again: {live:?}");
}

/// A write forgets what was kept: the lookup after it reads again.
#[tokio::test]
async fn a_write_forgets_the_kept_lookup() {
    use ferrocast_core::stream_info::StreamInfo;
    let info = StreamInfo { title: Some("A".into()), ..Default::default() };
    let (ep, calls) = start_scripted(a_channel_with_a_broadcast()).await;
    let api = video_api(ep);
    api.update_broadcast_info(&info).await.unwrap();
    // A broadcast is made on the vertical stream: lists change.
    api.vertical_ingest().await.unwrap();
    let before = reads(&calls.lock().unwrap(), "/liveStreams");
    api.set_thumbnail(b"x", "image/png").await.unwrap();
    let after = reads(&calls.lock().unwrap(), "/liveStreams");
    assert_eq!(after, before + 1, "the thumbnail's lookup read again after the writes: {:?}", calls.lock().unwrap());
}

/// The bot's polling for a live chat is never answered from what was kept.
#[tokio::test]
async fn looking_for_the_live_chat_is_never_answered_from_the_kept_lists() {
    let (ep, calls) = start_scripted(a_channel_with_a_broadcast()).await;
    let api = video_api(ep);
    api.active_live_chat().await.unwrap();
    api.active_live_chat().await.unwrap();
    assert_eq!(reads(&calls.lock().unwrap(), "/liveBroadcasts"), 2);
}

/// A whole Go live with a title, a thumbnail and the vertical stream, as the engine does it:
/// what it costs in YouTube's units with nothing kept, and now.
#[tokio::test]
async fn units_for_a_whole_go_live() {
    use ferrocast_core::stream_info::StreamInfo;
    let info = StreamInfo { title: Some("Ranked all night".into()), ..Default::default() };
    let go_live = |keep: Duration| {
        let info = info.clone();
        async move {
            let (ep, calls) = start_scripted(a_channel_with_a_broadcast()).await;
            let api = video_api(ep).with_read_cache(keep);
            api.update_broadcast_info(&info).await.unwrap();
            api.set_thumbnail(b"x", "image/png").await.unwrap();
            api.ingest().await.unwrap();
            api.vertical_ingest().await.unwrap();
            let log = calls.lock().unwrap().clone();
            (units(&log), log)
        }
    };
    let (before, _) = go_live(Duration::ZERO).await;
    let (after, log) = go_live(Duration::from_secs(60)).await;
    eprintln!("units for a whole Go live (title, thumbnail, normal and vertical): {before} before, {after} after");
    assert!(after < before, "{log:?}");
}

/// YouTube's second ingest for the same key rides along, of the same kind as the first (secure when
/// that is); one of another kind, or none, leaves the stream with just the first.
#[test]
fn the_backup_ingest_comes_with_the_stream_key() {
    let item = |info: serde_json::Value| {
        json!({ "items": [{ "id": "s1", "snippet": { "title": "Default", "isDefaultStream": true },
            "cdn": { "ingestionType": "rtmp", "resolution": "variable", "frameRate": "variable", "ingestionInfo": info } }] })
    };
    let secure = item(json!({
        "streamName": "k",
        "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com:443/live2",
        "rtmpsBackupIngestionAddress": "rtmps://b.rtmps.youtube.com:443/live2?backup=1",
        "ingestionAddress": "rtmp://a.rtmp.youtube.com/live2",
        "backupIngestionAddress": "rtmp://b.rtmp.youtube.com/live2?backup=1"
    }));
    let i = api::pick_stream(&secure, None).unwrap();
    assert_eq!(i.url, "rtmps://a.rtmps.youtube.com:443/live2");
    assert_eq!(i.backup.as_deref(), Some("rtmps://b.rtmps.youtube.com:443/live2?backup=1"));
    // Plain RTMP only: the plain backup.
    let plain = item(json!({
        "streamName": "k",
        "ingestionAddress": "rtmp://a.rtmp.youtube.com/live2",
        "backupIngestionAddress": "rtmp://b.rtmp.youtube.com/live2?backup=1"
    }));
    let i = api::pick_stream(&plain, None).unwrap();
    assert_eq!(i.backup.as_deref(), Some("rtmp://b.rtmp.youtube.com/live2?backup=1"));
    // A secure first server never gets a plain backup, and none given is none.
    let mixed = item(json!({
        "streamName": "k",
        "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com:443/live2",
        "backupIngestionAddress": "rtmp://b.rtmp.youtube.com/live2?backup=1"
    }));
    assert_eq!(api::pick_stream(&mixed, None).unwrap().backup, None);
    let alone = item(json!({ "streamName": "k", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com:443/live2" }));
    assert_eq!(api::pick_stream(&alone, None).unwrap().backup, None);
}

fn upcoming(id: &str, stream: &str, minutes_ahead: i64) -> serde_json::Value {
    json!({
        "id": id,
        "snippet": { "title": "Today", "scheduledStartTime": crate::api::rfc3339_utc(now_ms() + minutes_ahead * 60_000) },
        "status": { "lifeCycleStatus": "ready" },
        "contentDetails": { "boundStreamId": stream, "enableAutoStart": true }
    })
}

/// 20 Nov, a real account: the streamer's old key for vertical video is the channel's default, 1080p
/// fixed. The normal stream must go to Ferrocast's own variable key, not that one.
#[tokio::test]
async fn a_channel_whose_default_key_is_an_old_fixed_vertical_one_streams_to_ferrocasts_own() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s-old",
            "Vertical 1080p",
            true,
            "1080p",
            "variable",
            "rtmp",
            "old-vertical",
        )]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let ing = video_api(ep).ingest().await.unwrap();
    assert!(ing.key.starts_with("var-key-"), "not the old vertical key: {}", ing.key);
    assert_eq!(ing.note.as_deref(), Some("Ferrocast · variable"));
    let log = calls.lock().unwrap().clone();
    assert_eq!(log.iter().filter(|c| c.starts_with("POST /liveStreams ")).count(), 1, "{log:?}");
    assert!(log.iter().any(|c| c.starts_with("POST /liveBroadcasts ")), "a broadcast on the new key: {log:?}");
}

/// The same kind of channel with a broadcast already waiting, bound to a fixed 1080p key (named
/// nothing about vertical): the broadcast is bound to Ferrocast's variable key instead
/// (liveBroadcasts.bind).
#[tokio::test]
async fn an_upcoming_broadcast_bound_to_an_old_fixed_key_is_rebound_to_ferrocasts_own() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s-old",
            "Old key",
            true,
            "1080p",
            "variable",
            "rtmp",
            "old-1080p",
        )]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![upcoming("b1", "s-old", 10)]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let ing = video_api(ep).ingest().await.unwrap();
    assert!(ing.key.starts_with("var-key-"), "{}", ing.key);
    let log = calls.lock().unwrap().clone();
    assert!(log.iter().any(|c| c.starts_with("POST /liveBroadcasts/bind b1->s-new-")), "re-bound: {log:?}");
}

/// A broadcast waiting on the streamer's own vertical key (fixed 1080p, or variable) is their
/// vertical one: not taken for the normal live and not re-bound (that would take it from their
/// Shorts). The normal stream gets its own broadcast on Ferrocast's variable key.
#[tokio::test]
async fn a_broadcast_waiting_on_the_streamers_vertical_key_is_left_alone() {
    for (title, res) in [("Vertical 1080p", "1080p"), ("Shorts live", "variable")] {
        let st = ScriptedYoutube {
            stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
                "s-v", title, true, res, "variable", "rtmp", "v-key",
            )]])),
            broadcast_pages: Arc::new(Mutex::new(vec![vec![upcoming("b1", "s-v", 5)]])),
            ..Default::default()
        };
        let (ep, calls) = start_scripted(st).await;
        let ing = video_api(ep).ingest().await.unwrap();
        assert!(ing.key.starts_with("var-key-"), "{title}: {}", ing.key);
        assert_eq!(ing.note.as_deref(), Some("Ferrocast · variable"));
        let log = calls.lock().unwrap().clone();
        assert!(!log.iter().any(|c| c.contains("bind b1")), "{title}: their broadcast stays: {log:?}");
        assert!(log.iter().any(|c| c.starts_with("POST /liveBroadcasts ")), "{title}: its own broadcast: {log:?}");
    }
}

/// A waiting broadcast on the vertical key is the vertical one, never the normal live: the other
/// one, on a good key, is used as it is, with nothing created or bound.
#[tokio::test]
async fn a_broadcast_on_a_vertical_key_is_not_taken_for_the_normal_live() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![
            stream_item("s-v", "Vertical", false, "variable", "variable", "rtmp", "v-key"),
            stream_item("s-main", "Stream key", true, "variable", "variable", "rtmp", "main-key"),
        ]])),
        // The vertical one is the nearer in time.
        broadcast_pages: Arc::new(Mutex::new(vec![vec![upcoming("b-v", "s-v", 1), upcoming("b-main", "s-main", 30)]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let ing = video_api(ep).ingest().await.unwrap();
    assert_eq!(ing.key, "main-key");
    assert_eq!(ing.note.as_deref(), Some("Stream key · variable"));
    let log = calls.lock().unwrap().clone();
    assert!(!log.iter().any(|c| c.contains("bind") || c.starts_with("POST /liveStreams")), "{log:?}");
}

/// With a vertical default and a good key beside it, the good key is the one (no new key made).
#[tokio::test]
async fn a_good_key_beside_a_vertical_default_is_used_for_the_normal_stream() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![
            stream_item("s-v", "Vertical", true, "variable", "variable", "rtmp", "v-key"),
            stream_item("s-main", "Main", false, "variable", "variable", "rtmp", "main-key"),
        ]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st).await;
    let ing = video_api(ep).ingest().await.unwrap();
    assert_eq!(ing.key, "main-key");
    let log = calls.lock().unwrap().clone();
    assert!(!log.iter().any(|c| c.starts_with("POST /liveStreams")), "{log:?}");
}

#[test]
fn keys_made_for_vertical_video_are_told_by_their_words() {
    use crate::api::stream_unfit_for_main;
    let none = std::collections::HashSet::new();
    let key = |title: &str, desc: &str| {
        json!({ "id": "s", "snippet": { "title": title, "description": desc },
            "cdn": { "resolution": "variable", "frameRate": "variable", "ingestionType": "rtmp",
                "ingestionInfo": { "streamName": "k", "rtmpsIngestionAddress": "rtmps://a.rtmps.youtube.com/live2" } } })
    };
    assert!(!stream_unfit_for_main(&key("Stream key", ""), &none));
    assert!(!stream_unfit_for_main(&key("Ferrocast", "Ferrocast streams here, at any resolution."), &none));
    for (t, d) in [("My Vertical key", ""), ("phone", "for YouTube SHORTS"), ("9:16", ""), ("Portrait", "")] {
        assert!(stream_unfit_for_main(&key(t, d), &none), "{t} / {d}");
    }
    // Ferrocast's own vertical key is one too: it never carries the normal stream.
    assert!(stream_unfit_for_main(&key("Ferrocast vertical", ""), &none));
}

/// The hard signal: a key the vertical canvas is set to send to is never the normal one, whatever it
/// is called ("My key 2", the channel's default, variable): the normal stream goes to Ferrocast's own.
#[tokio::test]
async fn a_key_the_vertical_canvas_sends_to_is_never_the_normal_streams() {
    let st = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![
            stream_item("s-2", "My key 2", true, "variable", "variable", "rtmp", "vertical-canvas-key"),
            stream_item("s-3", "My key 3", false, "variable", "variable", "rtmp", "other-key"),
        ]])),
        broadcast_pages: Arc::new(Mutex::new(vec![vec![upcoming("b-v", "s-2", 2)]])),
        ..Default::default()
    };
    let (ep, calls) = start_scripted(st.clone()).await;
    let api = video_api(ep);
    // Without the signal a neutral name is taken as the normal key (and its broadcast).
    let before = api.ingest().await.unwrap();
    assert_eq!(before.key, "vertical-canvas-key");
    // With it: not the default, not its broadcast; the other good key carries the stream.
    api.set_vertical_keys(["vertical-canvas-key".to_string()]);
    let after = api.ingest().await.unwrap();
    assert_eq!(after.key, "other-key", "the other good key");
    assert_eq!(after.note.as_deref(), Some("My key 3 · variable"));
    let log = calls.lock().unwrap().clone();
    assert!(!log.iter().any(|c| c.contains("bind b-v")), "the vertical canvas's broadcast stays: {log:?}");
    // With no other key left, Ferrocast's own is made.
    let st2 = ScriptedYoutube {
        stream_pages: Arc::new(Mutex::new(vec![vec![stream_item(
            "s-2",
            "My key 2",
            true,
            "variable",
            "variable",
            "rtmp",
            "vertical-canvas-key",
        )]])),
        ..Default::default()
    };
    let (ep2, _) = start_scripted(st2).await;
    let api2 = video_api(ep2);
    api2.set_vertical_keys(["vertical-canvas-key".to_string()]);
    let own = api2.ingest().await.unwrap();
    assert!(own.key.starts_with("var-key-"), "{}", own.key);
    assert_eq!(own.note.as_deref(), Some("Ferrocast · variable"));
}

// ---------------------------------------------------------------- video upload (clips)

struct Bytes(Vec<u8>);

impl ferrocast_connector::UploadSource for Bytes {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let a = (offset as usize).min(self.0.len());
        Ok(self.0[a..(a + len).min(self.0.len())].to_vec())
    }
}

#[derive(Default)]
struct UploadLog {
    opened: Vec<(String, serde_json::Value, Option<String>, Option<String>)>,
    ranges: Vec<String>,
    got: usize,
    /// Answers to give for the next pieces: a status per request (200 when none are left).
    fail_next: Vec<u16>,
    /// Where the session address points ("" = this server).
    location: String,
    seen_auth: Vec<String>,
}

/// A fake resumable-upload server: opens a session, takes pieces, answers 308 until the end.
async fn fake_upload(log: Arc<Mutex<UploadLog>>) -> Endpoints {
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    let base_cell = Arc::new(Mutex::new(String::new()));
    let (l1, l2, b1, b2) = (log.clone(), log.clone(), base_cell.clone(), base_cell.clone());
    let app = axum::Router::new()
        .route(
            "/api/upload/videos",
            axum::routing::post(
                move |axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
                      h: HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let (l, b) = (l1.clone(), b1.clone());
                    async move {
                        let mut l = l.lock().unwrap();
                        let hv = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(String::from);
                        l.opened.push((
                            format!(
                                "{}|{}",
                                q.get("uploadType").cloned().unwrap_or_default(),
                                q.get("part").cloned().unwrap_or_default()
                            ),
                            body,
                            hv("x-upload-content-length"),
                            hv("authorization"),
                        ));
                        let loc = if l.location.is_empty() {
                            format!("{}/api/session/abc", b.lock().unwrap())
                        } else {
                            l.location.clone()
                        };
                        let mut out = HeaderMap::new();
                        out.insert("location", HeaderValue::from_str(&loc).unwrap());
                        (StatusCode::OK, out, String::new())
                    }
                },
            ),
        )
        .route(
            "/api/session/abc",
            axum::routing::put(move |h: HeaderMap, body: axum::body::Bytes| {
                let l = l2.clone();
                let _ = &b2;
                async move {
                    let mut l = l.lock().unwrap();
                    let range = h.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    l.seen_auth.push(h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                    l.ranges.push(range.clone());
                    if !l.fail_next.is_empty() {
                        let s = l.fail_next.remove(0);
                        return (StatusCode::from_u16(s).unwrap(), HeaderMap::new(), "{}".to_string());
                    }
                    let mut out = HeaderMap::new();
                    // "bytes */N": a question about how much is there.
                    if range.starts_with("bytes */") {
                        if l.got > 0 {
                            out.insert("range", HeaderValue::from_str(&format!("bytes=0-{}", l.got - 1)).unwrap());
                        }
                        return (StatusCode::PERMANENT_REDIRECT, out, String::new());
                    }
                    let (span, total) = range.trim_start_matches("bytes ").split_once('/').unwrap();
                    let (a, b) = span.split_once('-').unwrap();
                    let (a, b, total): (usize, usize, usize) =
                        (a.parse().unwrap(), b.parse().unwrap(), total.parse().unwrap());
                    assert_eq!(body.len(), b - a + 1);
                    l.got = b + 1;
                    if b + 1 == total {
                        (StatusCode::OK, out, "{\"id\":\"vid-new\"}".to_string())
                    } else {
                        out.insert("range", HeaderValue::from_str(&format!("bytes=0-{b}")).unwrap());
                        (StatusCode::PERMANENT_REDIRECT, out, String::new())
                    }
                }
            }),
        );
    // The pieces are 8 MB: past axum's default 2 MB.
    let app = app.layer(axum::extract::DefaultBodyLimit::disable());
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rest.local_addr().unwrap());
    *base_cell.lock().unwrap() = base.clone();
    tokio::spawn(async move { axum::serve(rest, app).await });
    Endpoints {
        authorize: format!("{base}/auth"),
        token: format!("{base}/token"),
        api: format!("{base}/api"),
        grpc: "http://127.0.0.1:9".into(),
    }
}

fn clip_meta() -> ferrocast_connector::VideoUpload {
    ferrocast_connector::VideoUpload {
        title: "Best play".into(),
        description: "from the stream".into(),
        tags: vec!["fps".into()],
        privacy: "unlisted".into(),
        category_id: Some("20".into()),
        made_for_kids: false,
    }
}

#[tokio::test]
async fn a_video_is_sent_in_pieces_with_its_details_and_reports_progress() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    let api = video_api(fake_upload(log.clone()).await);
    // A bit over two pieces.
    let size = 2 * super::api::UPLOAD_CHUNK_FOR_TEST + 1000;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let id = api
        .upload_video(&clip_meta(), &mut Bytes(vec![7u8; size]), &move |a, b| s2.lock().unwrap().push((a, b)))
        .await
        .unwrap();
    assert_eq!(id, "vid-new");
    let l = log.lock().unwrap();
    let (kind, body, len, auth) = &l.opened[0];
    assert_eq!(kind, "resumable|snippet,status");
    assert_eq!(len.as_deref(), Some(size.to_string().as_str()));
    assert_eq!(auth.as_deref(), Some("Bearer a"));
    assert_eq!(body["snippet"]["title"], "Best play");
    assert_eq!(body["snippet"]["categoryId"], "20");
    assert_eq!(body["status"]["privacyStatus"], "unlisted");
    assert_eq!(body["status"]["selfDeclaredMadeForKids"], false);
    assert_eq!(l.ranges.len(), 3, "{:?}", l.ranges);
    assert!(l.ranges[0].starts_with("bytes 0-") && l.ranges[2].ends_with(&format!("/{size}")));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.first(), Some(&(0, size as u64)));
    assert_eq!(seen.last(), Some(&(size as u64, size as u64)));
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "progress only goes forward: {seen:?}");
}

#[tokio::test]
async fn an_unknown_privacy_is_private_never_public() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    let api = video_api(fake_upload(log.clone()).await);
    let mut m = clip_meta();
    m.privacy = "everyone".into();
    api.upload_video(&m, &mut Bytes(vec![1; 10]), &|_, _| {}).await.unwrap();
    assert_eq!(log.lock().unwrap().opened[0].1["status"]["privacyStatus"], "private");
}

#[tokio::test]
async fn a_dropped_piece_resumes_where_youtube_left_off() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    log.lock().unwrap().fail_next = vec![503];
    let api = video_api(fake_upload(log.clone()).await);
    let size = super::api::UPLOAD_CHUNK_FOR_TEST + 500;
    let id = api.upload_video(&clip_meta(), &mut Bytes(vec![3u8; size]), &|_, _| {}).await.unwrap();
    assert_eq!(id, "vid-new");
    let l = log.lock().unwrap();
    // The first piece failed once (503), the question "how much do you have" got 0, and it went again.
    assert!(l.ranges.iter().any(|r| r.starts_with("bytes */")), "{:?}", l.ranges);
    assert_eq!(l.got, size);
}

#[tokio::test]
async fn too_many_failures_give_up_in_words() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    log.lock().unwrap().fail_next = vec![503; 20];
    let api = video_api(fake_upload(log.clone()).await);
    let err = api.upload_video(&clip_meta(), &mut Bytes(vec![3u8; 100]), &|_, _| {}).await.unwrap_err();
    assert!(matches!(err, YoutubeError::Refused(ref m) if m.contains("503")), "{err:?}");
}

#[tokio::test]
async fn a_refused_upload_says_why_and_an_empty_file_isnt_sent() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    log.lock().unwrap().fail_next = vec![403];
    let api = video_api(fake_upload(log.clone()).await);
    let err = api.upload_video(&clip_meta(), &mut Bytes(vec![3u8; 100]), &|_, _| {}).await.unwrap_err();
    assert!(matches!(err, YoutubeError::Refused(_)), "{err:?}");
    assert!(matches!(
        api.upload_video(&clip_meta(), &mut Bytes(Vec::new()), &|_, _| {}).await,
        Err(YoutubeError::Refused(_))
    ));
    assert_eq!(log.lock().unwrap().opened.len(), 1, "an empty file never opens a session");
}

#[tokio::test]
async fn the_file_and_sign_in_only_go_to_youtubes_upload_host() {
    let log = Arc::new(Mutex::new(UploadLog::default()));
    log.lock().unwrap().location = "https://evil.example.com/steal".into();
    let api = video_api(fake_upload(log.clone()).await);
    let err = api.upload_video(&clip_meta(), &mut Bytes(vec![3u8; 100]), &|_, _| {}).await.unwrap_err();
    assert!(matches!(err, YoutubeError::Protocol(_)), "{err:?}");
    assert!(log.lock().unwrap().ranges.is_empty(), "nothing was sent");
    assert!(super::api::upload_host_ok_for_test(
        "https://www.googleapis.com/upload/youtube/v3/videos?upload_id=x",
        "https://www.googleapis.com/youtube/v3"
    ));
    assert!(!super::api::upload_host_ok_for_test(
        "https://googleapis.com.evil.com/x",
        "https://www.googleapis.com/youtube/v3"
    ));
    assert!(!super::api::upload_host_ok_for_test(
        "http://www.googleapis.com/x",
        "https://www.googleapis.com/youtube/v3"
    ));
    assert!(!super::api::upload_host_ok_for_test("file:///etc/passwd", "https://www.googleapis.com/youtube/v3"));
}
