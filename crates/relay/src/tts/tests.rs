//! Pro voices: the pure parts, Google's client against a fake Google, then every route through
//! the real `router()` with `FakeTts` (every status each can answer), the quotas and the
//! relay-wide money guard.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const HINDI: &str = "google:hi-IN-Neural2-A";

// ── Unit ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_voice_list_starts_with_hindi_and_indian_english() {
    let v = google::voices();
    assert_eq!(v.len(), 10);
    assert!(v[..4].iter().all(|x| x.language == "hi-IN"), "{v:?}");
    assert!(v[4..8].iter().all(|x| x.language == "en-IN"), "{v:?}");
    assert_eq!(v.iter().filter(|x| x.language == "en-US").count(), 2);
    for lang in ["hi-IN", "en-IN"] {
        for family in ["Neural2", "WaveNet"] {
            let genders: Vec<_> =
                v.iter().filter(|x| x.language == lang && x.family == family).map(|x| x.gender).collect();
            assert_eq!(genders, ["female", "male"], "{lang} {family}");
        }
    }
    assert!(v.iter().all(|x| x.id.starts_with("google:")));
    assert_eq!(v[0].name, "Hindi, female (Neural2)");
    let names: std::collections::HashSet<_> = v.iter().map(|x| x.name.clone()).collect();
    assert_eq!(names.len(), v.len(), "names are unique in the picker");
}

#[test]
fn the_cap_never_goes_above_googles_free_tier() {
    assert_eq!(parse_cap(None), 800_000);
    assert_eq!(parse_cap(Some("  ")), 800_000);
    assert_eq!(parse_cap(Some("500,000")), 500_000);
    assert_eq!(parse_cap(Some("250_000")), 250_000);
    assert_eq!(parse_cap(Some("0")), 0, "0 rests the voices");
    assert_eq!(parse_cap(Some("5000000")), MAX_GLOBAL_MONTHLY);
    assert_eq!(parse_cap(Some("-3")), 800_000);
    assert_eq!(parse_cap(Some("lots")), 800_000);
    assert_eq!(Runtime::new(None, 9_000_000).global_monthly, MAX_GLOBAL_MONTHLY);
}

#[test]
fn the_relay_wide_minute_holds_sixty_requests() {
    let rt = Runtime::default();
    let t = 1_792_411_200_000;
    for _ in 0..GLOBAL_PER_MINUTE {
        assert!(rt.take_minute(t + 5));
    }
    assert!(!rt.take_minute(t + 59_999), "the 61st in the same minute");
    assert!(rt.take_minute(t + 60_000), "a new minute");
}

#[test]
fn text_loses_control_and_direction_characters() {
    assert_eq!(clean("  \u{202E}hi\u{0007}\nthere\u{200B} "), "hi  there");
    assert_eq!(clean("\u{200B}\u{0000} "), "");
}

#[test]
fn the_google_client_never_shows_its_key() {
    let g = GoogleTts::new("AIzaSecretKey123".into(), "http://127.0.0.1:1/x".into());
    let shown = format!("{g:?}");
    assert!(!shown.contains("AIzaSecretKey123") && shown.contains("redacted"), "{shown}");
}

// ── Google's client against a fake Google ────────────────────────────────────────────

#[derive(Debug, Default)]
struct FakeGoogle {
    /// Status and body to answer with.
    answer: Option<(u16, Value)>,
    seen: Vec<(Option<String>, String, Value)>,
}

type Shared = Arc<std::sync::Mutex<FakeGoogle>>;

async fn fake_google() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn synth(
        State(s): State<Shared>,
        headers: HeaderMap,
        uri: axum::http::Uri,
        axum::Json(body): axum::Json<Value>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        let mut f = s.lock().unwrap();
        let key = headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()).map(String::from);
        f.seen.push((key, uri.to_string(), body));
        let (status, v) = f.answer.clone().unwrap_or_else(|| {
            use base64::Engine as _;
            (200, json!({ "audioContent": base64::engine::general_purpose::STANDARD.encode(FAKE_OGG) }))
        });
        (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(v))
    }
    let app = axum::Router::new().route("/v1/text:synthesize", axum::routing::post(synth)).with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1/text:synthesize", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, shared)
}

#[tokio::test]
async fn google_gets_the_key_in_a_header_and_ogg_opus_is_asked_for() {
    let (url, g) = fake_google().await;
    let tts = GoogleTts::new("k-123".into(), url);
    let audio = tts.synthesize("नमस्ते दोस्तों", HINDI).await.unwrap();
    assert_eq!(audio.bytes, FAKE_OGG);
    assert_eq!(audio.mime, "audio/ogg");
    let seen = g.lock().unwrap().seen.clone();
    let (key, uri, body) = &seen[0];
    assert_eq!(key.as_deref(), Some("k-123"));
    assert!(!uri.contains("k-123") && !uri.contains("key="), "the key isn't in the URL: {uri}");
    assert_eq!(
        body,
        &json!({
            "input": { "text": "नमस्ते दोस्तों" },
            "voice": { "languageCode": "hi-IN", "name": "hi-IN-Neural2-A" },
            "audioConfig": { "audioEncoding": "OGG_OPUS" },
        })
    );
    // A voice outside the list never reaches Google.
    assert_eq!(tts.synthesize("hi", "google:xx-XX-Fake-A").await, Err(TtsError::BadVoice));
    assert_eq!(tts.synthesize("hi", "hi-IN-Neural2-A").await, Err(TtsError::BadVoice));
    assert_eq!(g.lock().unwrap().seen.len(), 1);
}

#[tokio::test]
async fn googles_errors_map_to_what_the_relay_can_do() {
    let (url, g) = fake_google().await;
    let tts = GoogleTts::new("k".into(), url);
    let cases = [
        (429, json!({ "error": { "message": "Quota exceeded" } }), TtsError::Quota),
        (503, json!({}), TtsError::Unavailable("Google answered 503".into())),
        (500, json!({}), TtsError::Unavailable("Google answered 500".into())),
        (
            403,
            json!({ "error": { "message": "API key not valid" } }),
            TtsError::Unavailable("Google refused the API key".into()),
        ),
        (400, json!({ "error": { "message": "Voice 'x' does not exist." } }), TtsError::BadVoice),
        (400, json!({ "error": { "message": "Input is longer than the limit" } }), TtsError::TooLong),
        (200, json!({ "audioContent": "bm90IG9nZw==" }), TtsError::Unavailable("audio that isn't Ogg".into())),
        (200, json!({ "nothing": true }), TtsError::Unavailable("no audio in the answer".into())),
    ];
    for (status, body, want) in cases {
        g.lock().unwrap().answer = Some((status, body));
        assert_eq!(tts.synthesize("hello", "google:en-IN-Neural2-A").await, Err(want), "{status}");
    }
    let down = GoogleTts::new("k".into(), "http://127.0.0.1:1/v1/text:synthesize".into());
    assert_eq!(down.synthesize("hello", HINDI).await, Err(TtsError::Unavailable("unreachable".into())));
}

// ── Through the router ───────────────────────────────────────────────────────────────

struct World {
    base: String,
    db: Arc<Db>,
    fake: Arc<FakeTts>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

async fn world() -> World {
    world_with(Some(DEFAULT_GLOBAL_MONTHLY), true).await
}

/// `cap`: the relay-wide monthly characters; `provider`: whether a voice provider is set up.
async fn world_with(cap: Option<i64>, provider: bool) -> World {
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
    let fake = Arc::new(FakeTts::new());
    let p: Option<Arc<dyn TtsProvider>> = provider.then(|| fake.clone() as Arc<dyn TtsProvider>);
    let state = AppState::new(cfg, db).with_tts(p, cap.unwrap_or(DEFAULT_GLOBAL_MONTHLY));
    let db = state.db.clone();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, db, fake, http: reqwest::Client::new(), dir }
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
        auth
    }

    async fn call(&self, method: reqwest::Method, path: &str, auth: Option<&str>, body: Option<Value>) -> Answer {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        Answer { status, headers, bytes: res.bytes().await.unwrap().to_vec() }
    }

    async fn speak(&self, auth: &str, text: &str, voice: &str) -> Answer {
        self.call(reqwest::Method::POST, "/v1/pro/tts", Some(auth), Some(json!({ "text": text, "voice": voice }))).await
    }

    async fn voices(&self, auth: &str) -> Answer {
        self.call(reqwest::Method::GET, "/v1/pro/tts/voices", Some(auth), None).await
    }

    fn used(&self, sub: &str) -> i64 {
        self.db.quota_used(sub, CHARS.name, CHARS.period).unwrap()
    }

    fn global(&self) -> i64 {
        self.db.tts_global_used(now_ms()).unwrap()
    }
}

/// Starts a test well inside a UTC minute, so per-minute counts can't roll over midway.
async fn early_in_a_minute() {
    let into = now_ms().rem_euclid(60_000);
    if into > 30_000 {
        tokio::time::sleep(std::time::Duration::from_millis(u64::try_from(60_100 - into).unwrap())).await;
    }
}

#[tokio::test]
async fn both_routes_need_a_known_install_with_pro() {
    let w = world().await;
    let free = w.install("free");
    for (m, path) in [(reqwest::Method::GET, "/v1/pro/tts/voices"), (reqwest::Method::POST, "/v1/pro/tts")] {
        let body = (m == reqwest::Method::POST).then(|| json!({ "nonsense": true }));
        let a = w.call(m.clone(), path, None, body.clone()).await;
        assert_eq!((a.status, a.code().as_str()), (401, "unauthorized"), "{path}");
        let a = w.call(m.clone(), path, Some("nobody.secret"), body.clone()).await;
        assert_eq!(a.status, 401, "{path}");
        let a = w.call(m, path, Some(&free), body).await;
        assert_eq!((a.status, a.code().as_str()), (402, "pro_required"), "{path}");
        assert!(a.json()["error"]["hint"].as_str().unwrap().contains("Settings › Pro"));
    }
    assert!(w.fake.calls().is_empty());
}

#[tokio::test]
async fn without_a_provider_the_feature_is_off_and_answers_503() {
    let w = world_with(None, false).await;
    let auth = w.pro("i1", "sub_1");
    let a = w.voices(&auth).await;
    assert_eq!((a.status, a.code().as_str()), (503, "tts_unavailable"));
    let a = w.speak(&auth, "hello", HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (503, "tts_unavailable"));
    assert!(a.json()["error"]["message"].as_str().unwrap().contains("aren't set up"));
    let s = w.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await.json();
    assert_eq!(s["features"]["tts"], false);
    assert_eq!(w.used("sub_1"), 0);
}

#[tokio::test]
async fn the_status_lists_the_feature_and_its_monthly_characters() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let s = w.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await.json();
    assert_eq!(s["features"]["tts"], true);
    assert_eq!(s["quotas"]["tts_chars"]["limit"], 20_000);
    assert_eq!(s["quotas"]["tts_chars"]["period"], "month");
    assert!(s["quotas"].get("tts_requests").is_none(), "a request rate isn't a quota");
}

#[tokio::test]
async fn voices_list_with_this_months_usage() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.db.try_take("sub_1", CHARS.name, CHARS.period, 4_120, CHARS.limit).unwrap();
    let a = w.voices(&auth).await;
    assert_eq!(a.status, 200);
    assert_eq!(a.headers["cache-control"], "no-store");
    let v = a.json();
    assert_eq!(v["voices"].as_array().unwrap().len(), 10);
    assert_eq!(
        v["voices"][0],
        json!({ "id": HINDI, "name": "Hindi, female (Neural2)", "language": "hi-IN", "gender": "female", "family": "Neural2" })
    );
    assert_eq!((v["usage"]["used"].as_i64(), v["usage"]["limit"].as_i64()), (Some(4_120), Some(20_000)));
    assert_eq!(v["usage"]["reset_at"].as_i64(), Some(Period::Month.reset_at(now_ms())));
    assert_eq!((v["resting"].as_bool(), v["max_chars"].as_u64()), (Some(false), Some(300)));
}

#[tokio::test]
async fn a_line_comes_back_as_ogg_and_counts_its_characters() {
    early_in_a_minute().await;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.speak(&auth, "  Thanks for the tip, Asha!  ", "google:en-IN-Neural2-B").await;
    assert_eq!(a.status, 200);
    assert_eq!(a.headers["content-type"], "audio/ogg");
    assert_eq!(a.headers["cache-control"], "no-store");
    assert_eq!(a.bytes, FAKE_OGG);
    assert_eq!(a.headers["x-tts-chars-used"], "25");
    assert_eq!(a.headers["x-tts-chars-limit"], "20000");
    assert_eq!(w.fake.calls(), [("Thanks for the tip, Asha!".to_string(), "google:en-IN-Neural2-B".to_string())]);
    assert_eq!((w.used("sub_1"), w.global()), (25, 25));
    // Characters, not bytes: Devanagari counts one per character.
    let a = w.speak(&auth, "धन्यवाद", HINDI).await;
    assert_eq!(a.status, 200);
    assert_eq!(w.used("sub_1"), 25 + "धन्यवाद".chars().count() as i64);
}

#[tokio::test]
async fn bad_requests_never_reach_the_provider_or_count() {
    early_in_a_minute().await;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let raw = |ct: &'static str, body: &'static str| {
        w.http.post(format!("{}/v1/pro/tts", w.base)).bearer_auth(&auth).header("content-type", ct).body(body).send()
    };
    let r = raw("text/plain", "hello").await.unwrap();
    assert_eq!(r.status(), 415);
    let r = raw("application/json", "{not json").await.unwrap();
    assert_eq!(r.status(), 400);
    let r = raw("application/json", "{\"text\":\"hi\"}").await.unwrap();
    assert_eq!(r.status(), 400, "no voice");

    let a = w.speak(&auth, " \u{200B}\n ", HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (422, "empty_text"));
    let a = w.speak(&auth, &"a".repeat(301), HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (413, "too_long"));
    assert!(a.json()["error"]["message"].as_str().unwrap().contains("300 characters"));
    let a = w.speak(&auth, &"अ".repeat(300), HINDI).await;
    assert_eq!(a.status, 200, "exactly 300 characters is fine");
    let a = w.speak(&auth, "hello", "google:xx-XX-Nope-A").await;
    assert_eq!((a.status, a.code().as_str()), (404, "voice_not_found"));
    let a = w.speak(&auth, "hello", "Samantha").await;
    assert_eq!(a.status, 404, "a system voice isn't a Ferrocast voice");
    assert_eq!(w.fake.calls().len(), 1, "only the good one");
    assert_eq!(w.used("sub_1"), 300);
}

#[tokio::test]
async fn the_monthly_characters_run_out_with_a_429_and_no_provider_call() {
    early_in_a_minute().await;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // Another PC on the same subscription used most of it.
    w.db.try_take("sub_1", CHARS.name, CHARS.period, 19_990, CHARS.limit).unwrap();
    let a = w.speak(&auth, "ten chars!", HINDI).await;
    assert_eq!(a.status, 200, "exactly the limit");
    let a = w.speak(&auth, "x", HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (429, "tts_quota"));
    let reset = Period::Month.reset_at(now_ms());
    assert_eq!(a.headers["x-quota-reset"], reset.to_string().as_str());
    let wait: i64 = a.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!(wait > 0 && wait <= (reset - now_ms()) / 1000 + 1, "{wait}");
    assert!(a.json()["error"]["hint"].as_str().unwrap().contains("starts again"));
    assert_eq!(w.fake.calls().len(), 1);
    assert_eq!(w.global(), 10, "the refused one didn't count relay-wide either");
    // Another subscription has its own month.
    let other = w.pro("i2", "sub_2");
    assert_eq!(w.speak(&other, "hello", HINDI).await.status, 200);
}

#[tokio::test]
async fn the_relay_wide_cap_rests_the_voices_and_never_calls_the_provider() {
    early_in_a_minute().await;
    let w = world_with(Some(100), true).await;
    let a1 = w.pro("i1", "sub_1");
    let a2 = w.pro("i2", "sub_2");
    assert_eq!(w.speak(&a1, &"a".repeat(60), HINDI).await.status, 200);
    // 60 + 50 would pass the cap: refused before Google, the subscription's count given back.
    let a = w.speak(&a2, &"b".repeat(50), HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (503, "tts_unavailable"));
    assert_eq!(a.json()["error"]["message"], "Ferrocast voices are resting until next month.");
    assert_eq!(a.headers["x-quota-reset"], Period::Month.reset_at(now_ms()).to_string().as_str());
    assert!(a.headers.contains_key("retry-after"));
    assert_eq!((w.used("sub_2"), w.global()), (0, 60));
    // Up to the cap exactly still works, then nothing more this month.
    assert_eq!(w.speak(&a2, &"c".repeat(40), HINDI).await.status, 200);
    assert_eq!(w.global(), 100);
    for _ in 0..3 {
        let a = w.speak(&a1, "x", HINDI).await;
        assert_eq!(a.status, 503);
    }
    assert_eq!(w.fake.calls().len(), 2, "the provider was never called once the cap was reached");
    assert_eq!(w.voices(&a1).await.json()["resting"], true);
    assert_eq!(w.used("sub_1"), 60);

    // A cap of 0 never calls it at all.
    let w = world_with(Some(0), true).await;
    let a = w.pro("i1", "sub_1");
    assert_eq!(w.speak(&a, "hello", HINDI).await.status, 503);
    assert!(w.fake.calls().is_empty());
}

#[tokio::test]
async fn request_rates_stop_runaway_loops() {
    // The windows are whole clock minutes, and 82 requests can take most of one on a slow CI
    // runner: a run that crosses a minute boundary proves nothing, so it starts over.
    for _ in 0..3 {
        early_in_a_minute().await;
        let minute = now_ms().div_euclid(60_000);
        let w = world().await;
        let subs: Vec<String> = (0..4).map(|i| w.pro(&format!("i{i}"), &format!("sub_{i}"))).collect();
        // One subscription: 20 a minute.
        let mut first = Vec::new();
        for _ in 0..SUB_RATE.limit {
            first.push(w.speak(&subs[0], "hi", HINDI).await.status);
        }
        let over = w.speak(&subs[0], "hi", HINDI).await;
        // Everyone together: 60 a minute.
        let mut rest = Vec::new();
        for s in &subs[1..3] {
            for _ in 0..SUB_RATE.limit {
                rest.push(w.speak(s, "hi", HINDI).await.status);
            }
        }
        let global = w.speak(&subs[3], "hi", HINDI).await;
        if now_ms().div_euclid(60_000) != minute {
            continue;
        }
        assert!(first.iter().all(|s| *s == 200), "{first:?}");
        assert_eq!((over.status, over.code().as_str()), (429, "tts_rate"));
        let wait: i64 = over.headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((1..=60).contains(&wait), "{wait}");
        assert!(rest.iter().all(|s| *s == 200), "{rest:?}");
        assert_eq!((global.status, global.code().as_str()), (429, "tts_rate"), "the 61st request this minute");
        assert_eq!(w.fake.calls().len(), 60);
        assert_eq!(w.used("sub_3"), 0);
        return;
    }
    panic!("three runs in a row crossed a minute boundary");
}

#[tokio::test]
async fn a_provider_failure_gives_the_characters_back() {
    early_in_a_minute().await;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // Down (or slow): 503, the subscription gets its characters back, and the relay-wide
    // count keeps them (Google may have billed a timed-out call).
    w.fake.fail_with(Some(TtsError::Unavailable("Google answered 500".into())));
    let a = w.speak(&auth, "hello there", HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (503, "tts_unavailable"));
    assert!(a.json()["error"]["hint"].as_str().unwrap().contains("own voice"));
    assert_eq!((w.used("sub_1"), w.global()), (0, 11));
    // Google's own quota: nothing made or billed, both come back.
    w.fake.fail_with(Some(TtsError::Quota));
    let a = w.speak(&auth, "hello", HINDI).await;
    assert_eq!((a.status, a.code().as_str()), (503, "tts_unavailable"));
    assert_eq!(a.headers["retry-after"], "60");
    assert_eq!((w.used("sub_1"), w.global()), (0, 11));
    w.fake.fail_with(Some(TtsError::BadVoice));
    assert_eq!(w.speak(&auth, "hello", HINDI).await.code(), "voice_not_found");
    w.fake.fail_with(Some(TtsError::TooLong));
    assert_eq!(w.speak(&auth, "hello", HINDI).await.code(), "too_long");
    assert_eq!((w.used("sub_1"), w.global()), (0, 11));
    w.fake.fail_with(None);
    assert_eq!(w.speak(&auth, "hello", HINDI).await.status, 200);
    assert_eq!(w.used("sub_1"), 5);
}

#[tokio::test]
async fn only_counters_are_stored_never_the_text() {
    early_in_a_minute().await;
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let secret = "MySecretTipMessage zebra-42";
    assert_eq!(w.speak(&auth, secret, "google:en-US-Neural2-F").await.status, 200);
    let copy = w.dir.path().join("copy.db");
    w.db.c().execute("VACUUM INTO ?1", [copy.to_str().unwrap()]).unwrap();
    let bytes = std::fs::read(&copy).unwrap();
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|x| x == needle);
    assert!(!has(secret.as_bytes()), "the text isn't stored");
    assert!(!has(b"en-US-Neural2-F"), "nor the voice");
    assert!(!has(&FAKE_OGG[28..36]), "nor the audio");
    let rows: i64 = w.db.c().query_row("SELECT COUNT(*) FROM tts_usage", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1);
}
