//! Spec-vs-router drift tests ensuring OpenAPI 3.1.0 and AsyncAPI 3.0.0 stay in 1:1 lockstep
//! with the live Axum router and engine frames.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use axum::body::Body as AxumBody;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;

fn server_src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn engine_src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("engine/src")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.contains("tests") {
                out.push(p);
            }
        }
    }
}

/// Parses every `.route("/path", methods)` call in `crates/server/src` (excluding non-REST UI/WS
/// files & routes) into normalized `("METHOD", "/api/v1/...")` pairs.
fn extract_router_rest_operations() -> BTreeSet<(String, String)> {
    let mut files = Vec::new();
    collect_rs_files(&server_src_dir(), &mut files);

    let mut ops = BTreeSet::new();
    for file in files {
        let rel = file.strip_prefix(server_src_dir()).unwrap().to_string_lossy().replace('\\', "/");
        if rel == "assets.rs" || rel == "ws.rs" || rel == "spec.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let bytes = text.as_bytes();
        let mut idx = 0;
        while let Some(pos) = text[idx..].find(".route(") {
            let start = idx + pos + 6;
            let mut depth = 0usize;
            let mut end = start;
            let mut in_str = false;
            let mut esc = false;
            while end < bytes.len() {
                let c = bytes[end];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'(' {
                    depth += 1;
                } else if c == b')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                end += 1;
            }
            let call = &text[start + 1..end];
            idx = end + 1;

            let Some(q1) = call.find('"') else { continue };
            let Some(q2_rel) = call[q1 + 1..].find('"') else { continue };
            let route_path = &call[q1 + 1..q1 + 1 + q2_rel];
            if route_path == "/live" || route_path == "/o/{token}/ws" {
                continue;
            }
            let rest = &call[q1 + 1 + q2_rel + 1..];
            for (needle, method) in
                [("get(", "GET"), ("post(", "POST"), ("put(", "PUT"), ("delete(", "DELETE"), ("patch(", "PATCH")]
            {
                if rest.contains(needle) {
                    ops.insert((method.to_string(), format!("/api/v1{route_path}")));
                }
            }
        }
    }
    ops
}

fn extract_spec_rest_operations(spec: &Value) -> BTreeSet<(String, String)> {
    let mut ops = BTreeSet::new();
    let paths = spec["paths"].as_object().expect("openapi.paths object");
    for (path, item) in paths {
        for (method, _) in item.as_object().expect("path item object") {
            let m = method.to_uppercase();
            if matches!(m.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                ops.insert((m, path.clone()));
            }
        }
    }
    ops
}

#[test]
fn openapi_3_1_matches_every_registered_router_route_bidirectionally() {
    let spec = spec::openapi();
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["components"]["securitySchemes"]["bearerAuth"].is_object());

    let code_ops = extract_router_rest_operations();
    let spec_ops = extract_spec_rest_operations(spec);

    let missing_in_spec: Vec<_> = code_ops.difference(&spec_ops).collect();
    let extra_in_spec: Vec<_> = spec_ops.difference(&code_ops).collect();

    assert!(
        missing_in_spec.is_empty(),
        "Routes exist in router but are MISSING from OpenAPI spec: {missing_in_spec:?}"
    );
    assert!(extra_in_spec.is_empty(), "Routes exist in OpenAPI spec but NOT in router source: {extra_in_spec:?}");
}

#[tokio::test]
async fn every_openapi_operation_dispatches_in_real_axum_router() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
    let sessions = Arc::new(Sessions::new());
    let desktop = sessions.desktop_token().to_string();
    let state = AppState {
        engine: engine.clone(),
        sessions,
        guard: auth::Guard { port: 47820, lan: false, dev_origins: vec![] },
        api_limiter: rate_limit::ApiKeyRateLimiter::default(),
    };
    let app = router(state);
    let ops = extract_spec_rest_operations(spec::openapi());

    for (method_str, path) in ops {
        let concrete = path
            .replace("{id}", "test_id")
            .replace("{platform}", "twitch")
            .replace("{user_id}", "user_1")
            .replace("{event_id}", "ev_1")
            .replace("{alert_id}", "al_1")
            .replace("{kind}", "collections")
            .replace("{scene}", "Main")
            .replace("{filter}", "Color")
            .replace("{name}", "Mic")
            .replace("{channel}", "desktop")
            .replace("{token}", "tok_1");
        let method = Method::from_bytes(method_str.as_bytes()).unwrap();
        let req = Request::builder()
            .method(method)
            .uri(&concrete)
            .header("host", "127.0.0.1:47820")
            .header("authorization", format!("Bearer {desktop}"))
            .body(AxumBody::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "Spec documents {method_str} {path}, which returned 405 from real router!"
        );
        if status == StatusCode::NOT_FOUND {
            let body = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            assert_ne!(
                v["error"]["code"], "no_route",
                "Spec documents {method_str} {path} ({concrete}), which hit router fallback no_route!"
            );
        }
    }

    engine.shutdown();
}

#[test]
fn asyncapi_3_0_covers_every_emitted_live_frame_and_overlay_frame() {
    let async_spec = spec::asyncapi();
    assert_eq!(async_spec["asyncapi"], "3.0.0");

    let live_msgs = async_spec["channels"]["live"]["messages"].as_object().unwrap();
    let overlay_msgs = async_spec["channels"]["overlay"]["messages"].as_object().unwrap();

    let mut files = Vec::new();
    collect_rs_files(&engine_src_dir(), &mut files);
    collect_rs_files(&server_src_dir(), &mut files);

    let mut live_kinds = BTreeSet::new();
    let mut overlay_kinds = BTreeSet::new();

    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for (marker, is_overlay) in [("LiveFrame::new(", false), ("studio_frame(", false), ("OverlayFrame::new(", true)]
        {
            let set = if is_overlay { &mut overlay_kinds } else { &mut live_kinds };
            let mut idx = 0;
            while let Some(pos) = compact[idx..].find(marker) {
                let tail = &compact[idx + pos + marker.len()..];
                idx += pos + marker.len();
                let target_quote = if is_overlay {
                    // OverlayFrame::new(audience, seq, "kind", data)
                    let mut commas = 0usize;
                    let mut found = None;
                    for (off, ch) in tail.char_indices() {
                        if ch == ',' {
                            commas += 1;
                            if commas == 2 {
                                found = tail[off + 1..].trim_start().strip_prefix('"');
                                break;
                            }
                        }
                    }
                    found
                } else {
                    tail.trim_start().strip_prefix('"')
                };
                if let Some(after_q) = target_quote
                    && let Some(end_q) = after_q.find('"')
                {
                    set.insert(after_q[..end_q].to_string());
                }
            }
        }
    }

    // `envelope(Some(event.seq), "event", &event)` in ws.rs + alert queue replay also emit these
    live_kinds.insert("event".into());
    live_kinds.insert("alert".into());

    for k in &live_kinds {
        assert!(
            live_msgs.contains_key(k) || k == "alert",
            "LiveFrame kind `{k}` emitted in code is MISSING from AsyncAPI `channels.live.messages`"
        );
    }
    for k in &overlay_kinds {
        assert!(
            overlay_msgs.contains_key(k),
            "OverlayFrame kind `{k}` emitted in code is MISSING from AsyncAPI `channels.overlay.messages`"
        );
    }
}

#[test]
fn checked_in_spec_files_match_generated_specs() {
    let root = repo_root();
    let openapi_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("docs/openapi.json")).unwrap()).unwrap();
    let asyncapi_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("docs/asyncapi.json")).unwrap()).unwrap();

    assert_eq!(&openapi_disk, spec::openapi(), "docs/openapi.json is out of date with spec::openapi()! Regenerate it.");
    assert_eq!(
        &asyncapi_disk,
        spec::asyncapi(),
        "docs/asyncapi.json is out of date with spec::asyncapi()! Regenerate it."
    );
}

fn msg_type(m: &Value) -> Option<&str> {
    m["payload"]["properties"]["type"]["const"].as_str()
}

#[test]
fn every_frame_matches_exactly_one_message_by_its_type() {
    let a = spec::asyncapi();
    for channel in ["live", "overlay"] {
        let msgs = a["channels"][channel]["messages"].as_object().unwrap();
        let mut seen = BTreeSet::new();
        for (name, m) in msgs {
            let t = msg_type(m)
                .unwrap_or_else(|| panic!("{channel}.{name} has no `type` constant: it would match any frame"));
            // The message is named by the frame it describes, except the overlay's inbound ones.
            let want = match name.as_str() {
                "inbound_ack" => "ack",
                "inbound_visibility" => "visibility",
                "inbound_hello" => "hello",
                other => other,
            };
            assert_eq!(t, want, "{channel}.{name}");
            assert!(seen.insert(t), "{channel}: two messages for type `{t}`");
            let required: Vec<&str> =
                m["payload"]["required"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            assert!(required.contains(&"type") && required.contains(&"data"), "{channel}.{name}: {required:?}");
        }
    }
    // The overlay's inbound frames carry their fields under `data`, as the server reads them.
    let ack = &a["channels"]["overlay"]["messages"]["inbound_ack"]["payload"]["properties"]["data"];
    assert_eq!(ack["required"], serde_json::json!(["alert_id", "phase"]));
    let vis = &a["channels"]["overlay"]["messages"]["inbound_visibility"]["payload"]["properties"]["data"];
    assert_eq!(vis["required"], serde_json::json!(["visible"]));
    // Overlay frames are numbered; the type constant is what tells the alert from the ack.
    assert!(
        a["channels"]["overlay"]["messages"]["alert"]["payload"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "seq")
    );
}

#[test]
fn the_live_handshake_query_is_in_the_websocket_binding_not_in_parameters() {
    let a = spec::asyncapi();
    let live = &a["channels"]["live"];
    assert!(live.get("parameters").is_none(), "/api/v1/live has no address placeholders");
    let q = &live["bindings"]["ws"]["query"];
    assert_eq!(q["type"], "object");
    for k in ["key", "session", "since"] {
        assert!(q["properties"][k].is_object(), "query.{k}");
    }
    assert_eq!(q["properties"]["since"]["type"], "integer");
    // The overlay's address does have a placeholder, and it stays a parameter.
    assert!(a["channels"]["overlay"]["parameters"]["token"].is_object());
}

fn op<'a>(o: &'a Value, path: &str, method: &str) -> &'a Value {
    &o["paths"][format!("/api/v1{path}")][method]
}

#[test]
fn uploads_are_described_as_multipart_with_their_fields() {
    let o = spec::openapi();
    for path in ["/designs/import", "/import", "/media"] {
        let c = &op(o, path, "post")["requestBody"]["content"];
        assert!(c.get("application/json").is_none(), "{path}: not json");
        let schema = &c["multipart/form-data"]["schema"];
        assert_eq!(schema["required"], serde_json::json!(["file"]), "{path}");
        assert_eq!(schema["properties"]["file"], serde_json::json!({ "type": "string", "format": "binary" }), "{path}");
    }
    let s = &op(o, "/import/streamlabs/preview", "post")["requestBody"]["content"]["multipart/form-data"]["schema"];
    for k in ["points", "tips"] {
        assert_eq!(s["properties"][k]["format"], "binary", "{k}");
    }
    assert_eq!(s["properties"]["platform"]["enum"], serde_json::json!(["twitch", "youtube", "kick"]));
    assert!(s.get("required").is_none(), "each field is optional");
}

#[test]
fn file_downloads_are_files_with_a_filename_and_keep_their_json_errors() {
    let o = spec::openapi();
    for (method, path, mime) in [
        ("post", "/designs/{id}/export", "application/zip"),
        ("post", "/export", "application/zip"),
        ("post", "/diagnostics/report.zip", "application/zip"),
        ("get", "/pro/backups/key", "text/plain"),
    ] {
        let r = &op(o, path, method)["responses"]["200"];
        let c = &r["content"];
        assert!(c.get("application/json").is_none(), "{path}: the answer is a file");
        assert_eq!(c[mime]["schema"], serde_json::json!({ "type": "string", "format": "binary" }), "{path}");
        assert!(r["headers"]["Content-Disposition"]["schema"]["type"] == "string", "{path}: names the file");
        assert!(
            op(o, path, method)["responses"]["401"]["content"]["application/json"].is_object(),
            "{path}: errors stay json"
        );
    }
    let csv = &op(o, "/moments/markers/export", "get")["responses"]["200"]["content"];
    assert!(csv["text/csv"].is_object() && csv["text/plain"].is_object());
}

#[test]
fn the_moderation_schemas_are_what_the_handlers_and_the_client_send() {
    let o = spec::openapi();
    let sch = &o["components"]["schemas"];
    assert_eq!(sch["ModTimeoutRequest"]["required"], serde_json::json!(["platform", "user_id", "seconds"]));
    assert!(sch["ModTimeoutRequest"]["properties"].get("duration_s").is_none());
    // ban: the user as an object, everywhere optional; unban: flat.
    assert_eq!(sch["ModBanRequest"]["required"], serde_json::json!(["user"]));
    assert_eq!(
        sch["ModBanRequest"]["properties"]["user"]["required"],
        serde_json::json!(["platform", "user_id", "login"])
    );
    assert_eq!(sch["ModBanRequest"]["properties"]["everywhere"]["type"], "boolean");
    assert_eq!(sch["ModUnbanRequest"]["required"], serde_json::json!(["platform", "user_id"]));
    assert_eq!(
        op(o, "/mod/unban", "post")["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ModUnbanRequest"
    );
    assert_eq!(
        op(o, "/mod/ban", "post")["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/BanResult"
    );
}

#[test]
fn the_schedule_routes_and_a_destinations_brand_are_described() {
    let o = spec::openapi();
    let post = &o["paths"]["/api/v1/schedule"]["post"];
    // The route answers 201 and takes a body: the spec says so (it said 200 and no body).
    assert!(post["responses"].get("201").is_some(), "{}", post["responses"]);
    assert!(post["responses"].get("200").is_none());
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ScheduleCreate"
    );
    let list = &o["paths"]["/api/v1/schedule"]["get"]["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(list["$ref"], "#/components/schemas/ScheduleList");
    let schemas = &o["components"]["schemas"];
    for name in ["ScheduleCreate", "ScheduledStream", "ScheduleList", "GoLiveDestinationCreate"] {
        assert!(schemas.get(name).is_some(), "{name} is not described");
    }
    assert_eq!(schemas["ScheduleCreate"]["properties"]["remind_minutes"]["maximum"], 1440);
    // The brand a pasted server can carry is in the request.
    let dest =
        &o["paths"]["/api/v1/golive/destinations"]["post"]["requestBody"]["content"]["application/json"]["schema"];
    assert_eq!(dest["$ref"], "#/components/schemas/GoLiveDestinationCreate");
    assert_eq!(
        schemas["GoLiveDestinationCreate"]["properties"]["brand"]["enum"],
        json!(["x", "instagram", "facebook"])
    );
    // SRT, RIST and WHIP servers need no key, so it is not required of every request.
    let required = schemas["GoLiveDestinationCreate"]["required"].as_array().unwrap();
    assert!(!required.iter().any(|r| r == "key"), "{required:?}");
    // A stream's category is `{ name, ids? }`, not a string.
    assert_eq!(schemas["ScheduleCreate"]["properties"]["info"]["properties"]["category"]["type"][0], "object");
}

#[test]
fn source_still_response_documents_optional_waiting_boolean() {
    let o = spec::openapi();
    let schema = &op(o, "/studio/engine/still", "get")["responses"]["200"]["content"]["application/json"]["schema"];
    let ref_path = schema["$ref"].as_str().expect("/studio/engine/still must reference a response schema");
    let name = ref_path.strip_prefix("#/components/schemas/").expect("schema ref");
    let def = &o["components"]["schemas"][name];
    let required = def["required"].as_array().expect("required fields");
    assert!(required.iter().any(|r| r == "image"), "{def}");
    assert!(
        !required.iter().any(|r| r == "waiting" || r == "error"),
        "waiting and error must be optional properties, not always required: {def}"
    );
    assert_eq!(def["properties"]["image"], serde_json::json!({ "type": ["string", "null"] }));
    assert_eq!(def["properties"]["waiting"]["type"], "boolean");
    assert_eq!(def["properties"]["error"], serde_json::json!({ "type": ["string", "null"] }));
}

#[test]
fn engine_stats_schema_documents_capture_problem_waiting_enum() {
    let o = spec::openapi();
    let schema = &op(o, "/studio/engine/stats", "get")["responses"]["200"]["content"]["application/json"]["schema"];
    let ref_path = schema["$ref"].as_str().expect("/studio/engine/stats must reference a response schema");
    let name = ref_path.strip_prefix("#/components/schemas/").expect("schema ref");
    let def = &o["components"]["schemas"][name];
    let captures_ref =
        def["properties"]["captures"]["items"]["$ref"].as_str().expect("captures items must reference CaptureProblem");
    let cp_name = captures_ref.strip_prefix("#/components/schemas/").expect("CaptureProblem ref");
    let cp = &o["components"]["schemas"][cp_name];
    assert_eq!(
        cp["properties"]["problem"]["enum"],
        serde_json::json!(["permission", "waiting", "no_picture", "black"])
    );
}

#[test]
fn engine_stats_schema_declares_collection_and_profile_ids() {
    let o = spec::openapi();
    let def = &o["components"]["schemas"]["StudioEngineStats"];
    assert_eq!(def["properties"]["collection_id"]["type"], serde_json::json!(["string", "null"]));
    assert_eq!(def["properties"]["profile_id"]["type"], serde_json::json!(["string", "null"]));
}

#[test]
fn screenshot_screen_still_and_source_status_contracts_match_runtime() {
    let o = spec::openapi();
    let schemas = &o["components"]["schemas"];
    fn resolve<'a>(schemas: &'a Value, node: &'a Value) -> &'a Value {
        if let Some(name) = node["$ref"].as_str().and_then(|r| r.strip_prefix("#/components/schemas/")) {
            &schemas[name]
        } else {
            node
        }
    }
    let find_query = |path: &str, method: &str, name: &str| -> Option<Value> {
        op(o, path, method)["parameters"]
            .as_array()
            .and_then(|ps| ps.iter().find(|p| p["in"] == "query" && p["name"] == name).cloned())
    };

    // 1. POST /studio/engine/sources/{name}/screenshot: optional scene+item query & body, {file} response.
    for param in ["scene", "item"] {
        let q = find_query("/studio/engine/sources/{name}/screenshot", "post", param)
            .unwrap_or_else(|| panic!("screenshot query param `{param}` missing"));
        assert_eq!(q["required"], json!(false), "{param} query must be optional");
    }
    let shot_op = op(o, "/studio/engine/sources/{name}/screenshot", "post");
    assert_eq!(shot_op["requestBody"]["required"], json!(false), "screenshot body is optional");
    let shot_req = resolve(schemas, &shot_op["requestBody"]["content"]["application/json"]["schema"]);
    assert_eq!(shot_req["properties"]["scene"]["type"], "string");
    assert_eq!(shot_req["properties"]["item"]["type"], "integer");
    let shot_resp = resolve(schemas, &shot_op["responses"]["200"]["content"]["application/json"]["schema"]);
    assert_eq!(shot_resp["required"], json!(["file"]));
    assert_eq!(shot_resp["properties"]["file"]["type"], "string");

    // 2. GET /studio/engine/screens/{id}/still uses StudioSourceStill, and both still endpoints document ?fresh=1.
    assert_eq!(
        op(o, "/studio/engine/screens/{id}/still", "get")["responses"]["200"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/StudioSourceStill"
    );
    for path in ["/studio/engine/still", "/studio/engine/screens/{id}/still"] {
        let fresh = find_query(path, "get", "fresh")
            .unwrap_or_else(|| panic!("{path} must document optional `fresh` query param"));
        assert_eq!(fresh["required"], json!(false), "{path}: fresh must be optional");
    }

    // 3. GET /studio/engine/sources/{name}/status documents the `camera` field.
    let status_resp = resolve(
        schemas,
        &op(o, "/studio/engine/sources/{name}/status", "get")["responses"]["200"]["content"]["application/json"]["schema"],
    );
    let cam = &status_resp["properties"]["camera"];
    assert_eq!(cam["type"], "object", "camera field missing: {status_resp}");
    assert_eq!(cam["properties"]["state"]["enum"], json!(["live", "waiting"]));
    assert_eq!(cam["properties"]["message"]["type"], json!(["string", "null"]));
    assert_eq!(cam["properties"]["tries"]["type"], "integer");
}

#[test]
fn creator_links_schema_allows_null_custom_and_documents_string_vs_array_clearing() {
    let o = spec::openapi();
    let schemas = &o["components"]["schemas"];
    assert_eq!(schemas["CreatorLinks"]["properties"]["custom"]["type"], json!(["array", "null"]));
    assert_eq!(schemas["LinksResponse"]["properties"]["custom"]["type"], "array");
    assert_eq!(schemas["ResolvedLinks"]["properties"]["custom"]["type"], "array");

    let put_summary = op(o, "/links", "put")["summary"].as_str().expect("PUT /links summary");
    assert!(put_summary.contains("string"), "summary must scope \"\" clearing to strings: {put_summary}");
    assert!(put_summary.contains("[]"), "summary must mention [] for clearing custom: {put_summary}");

    let desc = schemas["CreatorLinks"]["description"].as_str().expect("CreatorLinks description");
    assert!(desc.contains("string"), "description must scope \"\" clearing to strings: {desc}");
    assert!(desc.contains("[]"), "description must mention [] for clearing custom: {desc}");
}

#[test]
fn golive_quality_routes_and_additive_views_are_documented_in_openapi_and_asyncapi() {
    let o = spec::openapi();
    let schemas = &o["components"]["schemas"];

    let qo = op(o, "/golive/quality-options", "get");
    assert_eq!(qo["x-ferrocast-scopes"], json!(["read_only", "go_live"]));
    assert_eq!(
        qo["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/GoLiveQualityOptionsResponse"
    );

    let put_q = op(o, "/golive/destinations/{id}/quality", "put");
    assert_eq!(put_q["x-pc-only"], json!(true));
    assert_eq!(put_q["x-ferrocast-scopes"], json!([]));
    assert_eq!(
        put_q["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/SetDestinationQualityRequest"
    );
    assert_eq!(
        put_q["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/GoLiveView"
    );
    for code in ["400", "404", "409", "415", "422"] {
        assert!(put_q["responses"].get(code).is_some(), "missing {code} on PUT quality: {put_q}");
    }

    assert_eq!(
        op(o, "/golive", "get")["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/GoLiveView"
    );
    for f in ["encode_groups", "encode_budget"] {
        assert!(schemas["GoLiveView"]["properties"].get(f).is_some(), "GoLiveView missing {f}");
    }
    for f in ["quality", "resolved_quality", "switching_quality"] {
        assert!(schemas["DestinationView"]["properties"].get(f).is_some(), "DestinationView missing {f}");
    }
    let status_enum =
        schemas["EncodeBudgetCheck"]["properties"]["status"]["enum"].as_array().expect("EncodeBudgetCheck.status.enum");
    for s in [
        ferrocast_engine::golive::BudgetStatus::Ok,
        ferrocast_engine::golive::BudgetStatus::Limit,
        ferrocast_engine::golive::BudgetStatus::Over,
        ferrocast_engine::golive::BudgetStatus::Unknown,
    ] {
        let v = serde_json::to_value(s).unwrap();
        assert!(status_enum.contains(&v), "OpenAPI EncodeBudgetCheck.status missing {v}");
    }

    let a = spec::asyncapi();
    let async_status_enum = a["components"]["schemas"]["EncodeBudgetCheck"]["properties"]["status"]["enum"]
        .as_array()
        .expect("AsyncAPI EncodeBudgetCheck.status.enum");
    for s in [
        ferrocast_engine::golive::BudgetStatus::Ok,
        ferrocast_engine::golive::BudgetStatus::Limit,
        ferrocast_engine::golive::BudgetStatus::Over,
        ferrocast_engine::golive::BudgetStatus::Unknown,
    ] {
        let v = serde_json::to_value(s).unwrap();
        assert!(async_status_enum.contains(&v), "AsyncAPI EncodeBudgetCheck.status missing {v}");
    }
    let golive_data = &a["channels"]["live"]["messages"]["golive"]["payload"]["properties"]["data"];
    assert_eq!(golive_data["properties"]["encode_groups"]["items"]["$ref"], "#/components/schemas/EncodeGroupView");
    assert_eq!(golive_data["properties"]["encode_budget"]["$ref"], "#/components/schemas/EncodeBudgetCheck");
    for shared in ["DestinationView", "EncodeGroupView", "EncodeBudgetCheck", "ResolvedQuality", "DestinationQuality"] {
        assert!(a["components"]["schemas"].get(shared).is_some(), "AsyncAPI missing shared schema {shared}");
    }

    let get_eb = op(o, "/studio/engine/encode-budget", "get");
    assert_eq!(
        get_eb["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/EncodeBudgetResponse"
    );
    let post_eb = op(o, "/studio/engine/encode-budget", "post");
    assert_eq!(
        post_eb["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/EncodeBudgetRequest"
    );
    assert_eq!(
        post_eb["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/EncodeBudgetResponse"
    );
    for f in [
        "replay",
        "budget_mpx",
        "stream_mpx",
        "record_mpx",
        "shared",
        "why_not_shared",
        "level",
        "vertical_mpx",
        "used_mpx_s",
        "limit_mpx_s",
        "used_sessions",
        "max_sessions",
        "budget_source",
        "budget_status",
        "budget_note",
        "group_count",
    ] {
        assert!(schemas["EncodeBudgetResponse"]["properties"].get(f).is_some(), "EncodeBudgetResponse missing {f}");
    }
}
