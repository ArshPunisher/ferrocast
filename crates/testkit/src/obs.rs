//! A fake OBS (obs-websocket v5) for Studio tests: scenes with sources, audio inputs, the
//! replay buffer, GetStats and the stream output. Tests steer it through [`St`] and read
//! back what was asked. It never touches a real OBS: it listens on a random local port.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::any;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub source: String,
    pub enabled: bool,
    /// The input's kind; empty for a scene nested in this one.
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub struct Input {
    pub name: String,
    pub kind: String,
    pub muted: bool,
    /// Its settings (a browser source's `url`, `width`, `height`…).
    pub settings: Value,
}

#[derive(Debug, Default)]
pub struct FakeObs {
    /// Top first, as the OBS dock shows them.
    pub scenes: Vec<Scene>,
    pub current: String,
    pub inputs: Vec<Input>,
    pub replay_enabled: bool,
    pub replay_active: bool,
    pub replay_saves: u32,
    pub streaming: bool,
    /// OBS has no stream service or key set (StartStream answers 606).
    pub stream_unconfigured: bool,
    pub reconnecting: bool,
    pub stream_bytes: u64,
    pub stream_skipped: u64,
    pub stream_total: u64,
    pub cpu: f64,
    pub disk_mb: f64,
    pub render_skipped: u64,
    pub render_total: u64,
    /// Every request type, in order.
    pub requests: Vec<String>,
    /// Scene switches, in order.
    pub switches: Vec<String>,
    /// Close every connection at once (OBS quit).
    pub closed: bool,
    /// Settings › Stream: the service type and its settings.
    pub service_kind: String,
    pub service: Value,
    /// The current OBS profile's name.
    pub profile: String,
    /// The Aitum Multistream plugin's outputs and whether each is sending, when it's installed.
    pub aitum: Option<Vec<(String, bool)>>,
    /// Aitum Multistream 1.0.8 and earlier: its outputs run, but it doesn't answer vendor requests.
    pub aitum_old: bool,
    /// Aitum Vertical's first stream output, when it's installed.
    pub vertical: Option<FakeVertical>,
    /// Request types this OBS refuses (it answers 702, "processing failed").
    pub refuse: Vec<String>,
    /// SetSceneItemTransform calls: (scene, item id, transform).
    pub transforms: Vec<(String, i64, Value)>,
}

/// Aitum Vertical in the fake OBS: is the vertical canvas streaming, and where to.
#[derive(Debug, Default, Clone)]
pub struct FakeVertical {
    pub streaming: bool,
    pub server: String,
    pub key: String,
}

pub type St = Arc<Mutex<FakeObs>>;

fn uuid(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

impl FakeObs {
    /// A streamer's usual setup: Starting Soon, Gameplay (Mic, Webcam, Game), BRB, Ending.
    pub fn studio() -> St {
        let item = |id, source: &str, kind: &str| Item { id, source: source.into(), enabled: true, kind: kind.into() };
        let scene = |name: &str, items| Scene { name: name.into(), items };
        Arc::new(Mutex::new(FakeObs {
            scenes: vec![
                scene(
                    "Starting Soon",
                    vec![item(1, "Countdown", "browser_source"), item(2, "Mic", "wasapi_input_capture")],
                ),
                scene(
                    "Gameplay",
                    vec![
                        item(1, "Game", "game_capture"),
                        item(2, "Webcam", "dshow_input"),
                        item(3, "Mic", "wasapi_input_capture"),
                    ],
                ),
                scene("BRB", vec![item(1, "BRB card", "image_source")]),
                scene("Ending", vec![item(1, "Credits", "browser_source")]),
            ],
            current: "Starting Soon".into(),
            inputs: vec![
                Input { name: "Mic".into(), kind: "wasapi_input_capture".into(), muted: false, settings: Value::Null },
                Input { name: "Webcam".into(), kind: "dshow_input".into(), muted: false, settings: Value::Null },
                Input {
                    name: "Desktop Audio".into(),
                    kind: "wasapi_output_capture".into(),
                    muted: false,
                    settings: Value::Null,
                },
            ],
            replay_enabled: true,
            disk_mb: 200.0 * 1024.0,
            cpu: 8.0,
            // Streaming to Twitch through OBS's own list, as most people do.
            service_kind: "rtmp_common".into(),
            service: json!({ "service": "Twitch", "server": "auto", "key": "live_123_OldKey", "bwtest": false }),
            profile: "Untitled".into(),
            ..Default::default()
        }))
    }
}

/// Starts the fake on a random port and returns it.
pub async fn start(st: St) -> u16 {
    let app = Router::new().route("/", any(upgrade)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    port
}

/// A port nothing listens on (OBS closed).
pub async fn closed_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

async fn upgrade(State(st): State<St>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |ws| session(st, ws))
}

async fn session(st: St, mut ws: WebSocket) {
    let hello = json!({ "op": 0, "d": { "obsWebSocketVersion": "5.5.0", "rpcVersion": 1 } });
    if ws.send(Message::Text(hello.to_string().into())).await.is_err() {
        return;
    }
    // Identify, then Identified.
    let Some(Ok(_identify)) = ws.recv().await else { return };
    let ok = json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } });
    if ws.send(Message::Text(ok.to_string().into())).await.is_err() {
        return;
    }
    while let Some(Ok(msg)) = ws.recv().await {
        if st.lock().unwrap().closed {
            return;
        }
        let Message::Text(text) = msg else { continue };
        let Ok(req) = serde_json::from_str::<Value>(&text) else { continue };
        let d = &req["d"];
        let kind = d["requestType"].as_str().unwrap_or("").to_string();
        let (code, data) = handle(&st, &kind, &d["requestData"]);
        let mut status = json!({ "result": code == 100, "code": code });
        if code != 100 {
            status["comment"] = json!("fake OBS says no");
        }
        let mut resp =
            json!({ "op": 7, "d": { "requestType": kind, "requestId": d["requestId"], "requestStatus": status } });
        if !data.is_null() {
            resp["d"]["responseData"] = data;
        }
        if ws.send(Message::Text(resp.to_string().into())).await.is_err() {
            return;
        }
    }
}

/// Adds a source on top of a scene's list; its item id.
fn add_item(scene: &mut Scene, source: &str, kind: &str) -> i64 {
    let id = scene.items.iter().map(|it| it.id).max().unwrap_or(0) + 1;
    scene.items.push(Item { id, source: source.into(), enabled: true, kind: kind.into() });
    id
}

fn scene_index(s: &FakeObs, name: &str) -> Option<usize> {
    s.scenes.iter().position(|x| x.name == name)
}

fn handle(st: &St, kind: &str, data: &Value) -> (u16, Value) {
    let mut s = st.lock().unwrap();
    s.requests.push(kind.to_string());
    if s.refuse.iter().any(|r| r == kind) {
        return (702, Value::Null);
    }
    let name = |k: &str| data[k].as_str().unwrap_or("").to_string();
    let n = s.scenes.len();
    match kind {
        "GetVersion" => (
            100,
            json!({ "obsVersion": "32.0.1", "obsWebSocketVersion": "5.5.0", "rpcVersion": 1, "availableRequests": [],
                    "supportedImageFormats": [], "platform": "windows", "platformDescription": "Windows 11" }),
        ),
        "GetCurrentProgramScene" => {
            let i = scene_index(&s, &s.current).unwrap_or(0);
            (
                100,
                json!({ "sceneName": s.current, "sceneUuid": uuid(i + 1),
                        "currentProgramSceneName": s.current, "currentProgramSceneUuid": uuid(i + 1) }),
            )
        }
        "GetSceneList" => {
            let i = scene_index(&s, &s.current).unwrap_or(0);
            // OBS lists scenes bottom-up: index 0 is the bottom of the dock.
            let scenes: Vec<Value> = s
                .scenes
                .iter()
                .enumerate()
                .map(|(i, sc)| json!({ "sceneName": sc.name, "sceneUuid": uuid(i + 1), "sceneIndex": n - 1 - i }))
                .collect();
            (
                100,
                json!({ "currentProgramSceneName": s.current, "currentProgramSceneUuid": uuid(i + 1), "scenes": scenes }),
            )
        }
        "SetCurrentProgramScene" => {
            let target = name("sceneName");
            if scene_index(&s, &target).is_none() {
                return (600, Value::Null);
            }
            s.current = target.clone();
            s.switches.push(target);
            (100, Value::Null)
        }
        "GetSceneItemList" => match scene_index(&s, &name("sceneName")) {
            None => (600, Value::Null),
            Some(i) => {
                let items: Vec<Value> = s.scenes[i]
                    .items
                    .iter()
                    .enumerate()
                    .map(|(idx, it)| {
                        let nested = it.kind.is_empty();
                        json!({ "sceneItemId": it.id, "sceneItemIndex": idx, "sourceName": it.source,
                                "sourceType": if nested { "OBS_SOURCE_TYPE_SCENE" } else { "OBS_SOURCE_TYPE_INPUT" },
                                "inputKind": if nested { Value::Null } else { json!(it.kind) }, "isGroup": null,
                                "sceneItemEnabled": it.enabled })
                    })
                    .collect();
                (100, json!({ "sceneItems": items }))
            }
        },
        "GetSceneItemEnabled" | "SetSceneItemEnabled" => {
            let Some(i) = scene_index(&s, &name("sceneName")) else { return (600, Value::Null) };
            let id = data["sceneItemId"].as_i64().unwrap_or(-1);
            let Some(item) = s.scenes[i].items.iter_mut().find(|it| it.id == id) else { return (600, Value::Null) };
            if kind == "SetSceneItemEnabled" {
                item.enabled = data["sceneItemEnabled"].as_bool().unwrap_or(item.enabled);
                (100, Value::Null)
            } else {
                (100, json!({ "sceneItemEnabled": item.enabled }))
            }
        }
        "GetInputList" => {
            let only = name("inputKind");
            let inputs: Vec<Value> = s
                .inputs
                .iter()
                .enumerate()
                .filter(|(_, x)| only.is_empty() || x.kind == only)
                .map(|(i, x)| {
                    json!({ "inputName": x.name, "inputUuid": uuid(100 + i), "inputKind": x.kind, "unversionedInputKind": x.kind })
                })
                .collect();
            (100, json!({ "inputs": inputs }))
        }
        "CreateScene" => {
            let n = name("sceneName");
            if scene_index(&s, &n).is_some() {
                return (601, Value::Null);
            }
            // A new scene goes to the top of the dock.
            s.scenes.insert(0, Scene { name: n, items: Vec::new() });
            (100, json!({ "sceneUuid": uuid(900 + s.scenes.len()) }))
        }
        "SetSceneName" => {
            let (from, to) = (name("sceneName"), name("newSceneName"));
            if scene_index(&s, &to).is_some() {
                return (601, Value::Null);
            }
            let Some(i) = scene_index(&s, &from) else { return (600, Value::Null) };
            s.scenes[i].name = to.clone();
            for sc in &mut s.scenes {
                for it in &mut sc.items {
                    if it.kind.is_empty() && it.source == from {
                        it.source = to.clone();
                    }
                }
            }
            if s.current == from {
                s.current = to;
            }
            (100, Value::Null)
        }
        "SetInputName" => {
            let (from, to) = (name("inputName"), name("newInputName"));
            if s.inputs.iter().any(|x| x.name == to) {
                return (601, Value::Null);
            }
            let Some(x) = s.inputs.iter_mut().find(|x| x.name == from) else { return (600, Value::Null) };
            x.name = to.clone();
            for sc in &mut s.scenes {
                for it in &mut sc.items {
                    if !it.kind.is_empty() && it.source == from {
                        it.source = to.clone();
                    }
                }
            }
            (100, Value::Null)
        }
        "RemoveScene" => match scene_index(&s, &name("sceneName")) {
            None => (600, Value::Null),
            Some(i) => {
                s.scenes.remove(i);
                (100, Value::Null)
            }
        },
        "CreateInput" => {
            let (scene, input, kind) = (name("sceneName"), name("inputName"), name("inputKind"));
            if s.inputs.iter().any(|x| x.name == input) {
                return (601, Value::Null);
            }
            let Some(i) = scene_index(&s, &scene) else { return (600, Value::Null) };
            s.inputs.push(Input {
                name: input.clone(),
                kind: kind.clone(),
                muted: false,
                settings: data["inputSettings"].clone(),
            });
            let id = add_item(&mut s.scenes[i], &input, &kind);
            (100, json!({ "inputUuid": uuid(200 + s.inputs.len()), "sceneItemId": id }))
        }
        "GetInputSettings" => match s.inputs.iter().find(|x| x.name == name("inputName")) {
            None => (600, Value::Null),
            Some(x) => (
                100,
                json!({ "inputSettings": if x.settings.is_null() { json!({}) } else { x.settings.clone() }, "inputKind": x.kind }),
            ),
        },
        "SetInputSettings" => {
            let input = name("inputName");
            let Some(x) = s.inputs.iter_mut().find(|x| x.name == input) else { return (600, Value::Null) };
            let mut merged = if x.settings.is_object() { x.settings.clone() } else { json!({}) };
            if let (Some(m), Some(new)) = (merged.as_object_mut(), data["inputSettings"].as_object()) {
                m.extend(new.clone());
            }
            x.settings = merged;
            (100, Value::Null)
        }
        "RemoveInput" => {
            let input = name("inputName");
            if !s.inputs.iter().any(|x| x.name == input) {
                return (600, Value::Null);
            }
            s.inputs.retain(|x| x.name != input);
            for sc in &mut s.scenes {
                sc.items.retain(|it| it.source != input);
            }
            (100, Value::Null)
        }
        "CreateSceneItem" => {
            let (scene, source) = (name("sceneName"), name("sourceName"));
            let kind = match (s.inputs.iter().find(|x| x.name == source), scene_index(&s, &source)) {
                (Some(x), _) => x.kind.clone(),
                (None, Some(_)) => String::new(),
                (None, None) => return (600, Value::Null),
            };
            let Some(i) = scene_index(&s, &scene) else { return (600, Value::Null) };
            let id = add_item(&mut s.scenes[i], &source, &kind);
            (100, json!({ "sceneItemId": id }))
        }
        "RemoveSceneItem" => {
            let Some(i) = scene_index(&s, &name("sceneName")) else { return (600, Value::Null) };
            let id = data["sceneItemId"].as_i64().unwrap_or(-1);
            s.scenes[i].items.retain(|it| it.id != id);
            (100, Value::Null)
        }
        "SetSceneItemTransform" => {
            let scene = name("sceneName");
            let Some(i) = scene_index(&s, &scene) else { return (600, Value::Null) };
            let id = data["sceneItemId"].as_i64().unwrap_or(-1);
            if !s.scenes[i].items.iter().any(|it| it.id == id) {
                return (600, Value::Null);
            }
            s.transforms.push((scene, id, data["sceneItemTransform"].clone()));
            (100, Value::Null)
        }
        "SetSceneItemIndex" => {
            let Some(i) = scene_index(&s, &name("sceneName")) else { return (600, Value::Null) };
            let id = data["sceneItemId"].as_i64().unwrap_or(-1);
            let items = &mut s.scenes[i].items;
            let Some(from) = items.iter().position(|it| it.id == id) else { return (600, Value::Null) };
            let it = items.remove(from);
            let to = (data["sceneItemIndex"].as_u64().unwrap_or(0) as usize).min(items.len());
            items.insert(to, it);
            (100, Value::Null)
        }
        "GetInputMute" | "SetInputMute" => {
            let target = name("inputName");
            let Some(input) = s.inputs.iter_mut().find(|x| x.name == target) else { return (600, Value::Null) };
            if kind == "SetInputMute" {
                input.muted = data["inputMuted"].as_bool().unwrap_or(input.muted);
                (100, Value::Null)
            } else {
                (100, json!({ "inputMuted": input.muted }))
            }
        }
        "GetReplayBufferStatus" => (100, json!({ "outputActive": s.replay_active })),
        "StartReplayBuffer" => {
            if !s.replay_enabled {
                return (604, Value::Null);
            }
            if s.replay_active {
                return (500, Value::Null);
            }
            s.replay_active = true;
            (100, Value::Null)
        }
        "SaveReplayBuffer" => {
            if !s.replay_active {
                return (501, Value::Null);
            }
            s.replay_saves += 1;
            (100, Value::Null)
        }
        "GetLastReplayBufferReplay" => {
            (100, json!({ "savedReplayPath": format!("/videos/Replay {}.mkv", s.replay_saves) }))
        }
        "GetStats" => {
            s.render_total += 300;
            (
                100,
                json!({ "cpuUsage": s.cpu, "memoryUsage": 512.0, "availableDiskSpace": s.disk_mb, "activeFps": 60.0,
                        "averageFrameRenderTime": 2.5, "renderSkippedFrames": s.render_skipped, "renderTotalFrames": s.render_total,
                        "outputSkippedFrames": 0, "outputTotalFrames": s.render_total,
                        "webSocketSessionIncomingMessages": 1, "webSocketSessionOutgoingMessages": 1 }),
            )
        }
        "GetStreamServiceSettings" => {
            (100, json!({ "streamServiceType": s.service_kind, "streamServiceSettings": s.service }))
        }
        // As obs-websocket: refused while streaming; a new type replaces, the same type merges.
        "SetStreamServiceSettings" if s.streaming => (500, Value::Null),
        "SetStreamServiceSettings" => {
            let kind = data["streamServiceType"].as_str().unwrap_or("").to_string();
            let new = data["streamServiceSettings"].clone();
            if kind == s.service_kind {
                if let (Some(old), Some(new)) = (s.service.as_object_mut(), new.as_object()) {
                    for (k, v) in new {
                        old.insert(k.clone(), v.clone());
                    }
                }
            } else {
                s.service_kind = kind;
                s.service = new;
            }
            (100, Value::Null)
        }
        "StartStream" if s.stream_unconfigured => (606, Value::Null),
        "StartStream" if s.streaming => (500, Value::Null),
        "StartStream" => {
            s.streaming = true;
            (100, Value::Null)
        }
        "StopStream" if !s.streaming => (501, Value::Null),
        "StopStream" => {
            s.streaming = false;
            (100, Value::Null)
        }
        "GetStreamStatus" => (
            100,
            json!({ "outputActive": s.streaming, "outputReconnecting": s.reconnecting, "outputTimecode": "00:10:00.000",
                    "outputDuration": 600_000, "outputCongestion": 0.0, "outputBytes": s.stream_bytes,
                    "outputSkippedFrames": s.stream_skipped, "outputTotalFrames": s.stream_total }),
        ),
        "GetVideoSettings" => (
            100,
            json!({ "fpsNumerator": 60, "fpsDenominator": 1, "baseWidth": 2560, "baseHeight": 1440,
                    "outputWidth": 1920, "outputHeight": 1080 }),
        ),
        "GetSourceScreenshot" if s.scenes.iter().any(|x| x.name == name("sourceName")) => {
            (100, json!({ "imageData": "data:image/jpg;base64,/9j/4AAQ" }))
        }
        "GetSourceScreenshot" => (600, Value::Null),
        "GetProfileList" => (100, json!({ "currentProfileName": s.profile, "profiles": [s.profile] })),
        "GetOutputList" => {
            let mut outputs = vec![
                json!({ "outputName": "simple_stream", "outputKind": "rtmp_output", "outputWidth": 1920,
                "outputHeight": 1080, "outputActive": s.streaming, "outputFlags": { "OBS_OUTPUT_AUDIO": true,
                "OBS_OUTPUT_VIDEO": true, "OBS_OUTPUT_ENCODED": true, "OBS_OUTPUT_MULTI_TRACK": false, "OBS_OUTPUT_SERVICE": true } }),
            ];
            for (name, live) in s.aitum.iter().flatten() {
                outputs.push(json!({ "outputName": format!("aitum_multi_output_{name}"), "outputKind": "rtmp_output",
                    "outputWidth": 1920, "outputHeight": 1080, "outputActive": live, "outputFlags": { "OBS_OUTPUT_AUDIO": true,
                    "OBS_OUTPUT_VIDEO": true, "OBS_OUTPUT_ENCODED": true, "OBS_OUTPUT_MULTI_TRACK": false, "OBS_OUTPUT_SERVICE": true } }));
            }
            (100, json!({ "outputs": outputs }))
        }
        // obs-websocket answers an unknown vendor with 604 (ResourceNotFound family).
        "CallVendorRequest" => match (&s.aitum, name("vendorName").as_str(), name("requestType").as_str()) {
            (Some(list), "aitum-multistream", "get_outputs") if !s.aitum_old => {
                let outputs: Vec<Value> = list.iter().map(|(n, _)| json!({ "name": n, "vertical": false })).collect();
                (
                    100,
                    json!({ "vendorName": "aitum-multistream", "requestType": "get_outputs",
                              "responseData": { "outputs": outputs, "success": true } }),
                )
            }
            _ => match (&mut s.vertical, name("vendorName").as_str(), name("requestType").as_str()) {
                (Some(v), "aitum-vertical-canvas", rt) => {
                    let rd = &data["requestData"];
                    let response = match rt {
                        "status" => json!({ "success": true, "streaming": v.streaming }),
                        "update_stream_server" => {
                            v.server = rd["stream_server"].as_str().unwrap_or("").to_string();
                            json!({ "success": true })
                        }
                        "update_stream_key" => {
                            v.key = rd["stream_key"].as_str().unwrap_or("").to_string();
                            json!({ "success": true })
                        }
                        _ => json!({ "success": false }),
                    };
                    (100, json!({ "vendorName": "aitum-vertical-canvas", "requestType": rt, "responseData": response }))
                }
                _ => (604, Value::Null),
            },
        },
        _ => (204, Value::Null),
    }
}
