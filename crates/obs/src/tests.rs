//! A tiny fake obs-websocket v5 server, enough for "Add to OBS".

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::*;

#[derive(Default)]
struct FakeObs {
    scene: (String, String),
    /// (name, uuid, settings)
    inputs: Vec<(String, String, Value)>,
    /// (scene uuid, input uuid)
    items: Vec<(String, String)>,
    require_password: bool,
}

async fn fake(state: Arc<Mutex<FakeObs>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let auth = state.lock().unwrap().require_password;
                let mut hello =
                    json!({ "op": 0, "d": { "obsWebSocketVersion": "5.5.0", "rpcVersion": 1 } });
                if auth {
                    hello["d"]["authentication"] = json!({ "challenge": "c", "salt": "s" });
                }
                ws.send(Message::text(hello.to_string())).await.unwrap();
                let identify: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                if auth {
                    // Whatever was sent, it isn't the right answer in this fake.
                    let _ = identify;
                    let frame = CloseFrame {
                        code: CloseCode::from(4009),
                        reason: "Authentication failed.".into(),
                    };
                    let _ = ws.send(Message::Close(Some(frame))).await;
                    return;
                }
                ws.send(Message::text(
                    json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } }).to_string(),
                ))
                .await
                .unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    let Ok(text) = msg.to_text() else { continue };
                    let Ok(req) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    let d = &req["d"];
                    let (ok, data) = handle(
                        &state,
                        d["requestType"].as_str().unwrap_or(""),
                        &d["requestData"],
                    );
                    let mut resp = json!({ "op": 7, "d": {
                        "requestType": d["requestType"], "requestId": d["requestId"],
                        "requestStatus": if ok { json!({ "result": true, "code": 100 }) } else { json!({ "result": false, "code": 600, "comment": "not found" }) },
                    }});
                    // Real OBS leaves responseData out when a request returns nothing.
                    if !data.is_null() {
                        resp["d"]["responseData"] = data;
                    }
                    if ws.send(Message::text(resp.to_string())).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

fn handle(state: &Arc<Mutex<FakeObs>>, kind: &str, data: &Value) -> (bool, Value) {
    let mut s = state.lock().unwrap();
    let (scene_name, scene_uuid) = s.scene.clone();
    match kind {
        "GetVersion" => (
            true,
            json!({ "obsVersion": "32.0.1", "obsWebSocketVersion": "5.5.0", "rpcVersion": 1, "availableRequests": [],
                    "supportedImageFormats": [], "platform": "macos", "platformDescription": "macOS" }),
        ),
        "GetCurrentProgramScene" => (
            true,
            json!({ "sceneName": scene_name, "sceneUuid": scene_uuid,
                    "currentProgramSceneName": scene_name, "currentProgramSceneUuid": scene_uuid }),
        ),
        "GetInputList" => (
            true,
            json!({ "inputs": s.inputs.iter().map(|(n, u, _)| json!({ "inputName": n, "inputUuid": u,
                    "inputKind": "browser_source", "unversionedInputKind": "browser_source" })).collect::<Vec<_>>() }),
        ),
        "CreateInput" => {
            let uuid = format!("00000000-0000-4000-8000-{:012}", s.inputs.len() + 1);
            let name = data["inputName"].as_str().unwrap_or("").to_string();
            s.inputs
                .push((name, uuid.clone(), data["inputSettings"].clone()));
            s.items.push((scene_uuid, uuid.clone()));
            (
                true,
                json!({ "inputUuid": uuid, "sceneItemId": s.items.len() }),
            )
        }
        "SetInputSettings" => {
            let uuid = data["inputUuid"].as_str().unwrap_or("");
            if let Some(i) = s.inputs.iter_mut().find(|(_, u, _)| u == uuid) {
                i.2 = data["inputSettings"].clone();
                (true, Value::Null)
            } else {
                (false, Value::Null)
            }
        }
        "GetSceneItemId" => {
            let name = data["sourceName"].as_str().unwrap_or("");
            let found = s
                .inputs
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, u, _)| u.clone());
            match found.filter(|u| s.items.iter().any(|(sc, iu)| *sc == scene_uuid && iu == u)) {
                Some(_) => (true, json!({ "sceneItemId": 1 })),
                None => (false, Value::Null),
            }
        }
        "CreateSceneItem" => {
            s.items.push((
                scene_uuid,
                data["sourceUuid"].as_str().unwrap_or("").to_string(),
            ));
            (true, json!({ "sceneItemId": s.items.len() }))
        }
        _ => (false, Value::Null),
    }
}

fn state() -> Arc<Mutex<FakeObs>> {
    Arc::new(Mutex::new(FakeObs {
        scene: (
            "Gaming".into(),
            "11111111-1111-4111-8111-111111111111".into(),
        ),
        ..Default::default()
    }))
}

#[tokio::test]
async fn adds_then_updates_in_place_then_adds_to_a_new_scene() {
    let st = state();
    let port = fake(st.clone()).await;
    let cfg = ObsConfig {
        host: "127.0.0.1".into(),
        port,
    };
    let client = connect(&cfg, None).await.unwrap();
    assert_eq!(status(&client).await.unwrap().scene, "Gaming");
    let name = source_name("Main");
    let first = add_browser_source(&client, &name, "http://127.0.0.1:47820/o/aaa/", 1920, 1080)
        .await
        .unwrap();
    assert!(first.created);
    assert_eq!(first.scene, "Gaming");
    let second = add_browser_source(&client, &name, "http://127.0.0.1:47820/o/bbb/", 1280, 720)
        .await
        .unwrap();
    assert!(!second.created, "same name updates the existing source");
    {
        let s = st.lock().unwrap();
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.inputs[0].2["url"], "http://127.0.0.1:47820/o/bbb/");
        assert_eq!(s.inputs[0].2["width"], 1280);
        assert_eq!(s.inputs[0].2["shutdown"], false);
    }
    st.lock().unwrap().scene = (
        "Just Chatting".into(),
        "22222222-2222-4222-8222-222222222222".into(),
    );
    let third = add_browser_source(&client, &name, "http://127.0.0.1:47820/o/bbb/", 1920, 1080)
        .await
        .unwrap();
    assert_eq!(third.scene, "Just Chatting");
    assert_eq!(
        st.lock().unwrap().items.len(),
        2,
        "added to the new scene too"
    );
}

#[tokio::test]
async fn clear_errors_for_a_closed_obs_and_a_wrong_password() {
    let free = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = ObsConfig {
        host: "127.0.0.1".into(),
        port: free,
    };
    assert_eq!(
        connect(&cfg, None).await.err(),
        Some(ObsError::NotReachable)
    );
    let st = state();
    st.lock().unwrap().require_password = true;
    let port = fake(st).await;
    let cfg = ObsConfig {
        host: "127.0.0.1".into(),
        port,
    };
    let err = connect(&cfg, Some("wrong")).await.err();
    assert!(
        matches!(err, Some(ObsError::WrongPassword | ObsError::NotReachable)),
        "{err:?}"
    );
}

#[test]
fn source_names_are_stable_and_clean() {
    assert_eq!(source_name(" Main "), "Ferrocast: Main");
    assert_eq!(source_name("a\u{0}b\nc"), "Ferrocast: abc");
    assert!(source_name(&"x".repeat(500)).len() < 80);
}
