use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrocast_store::studio_library;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use super::*;
use crate::fonts::{FontStatus, fonts_dir};
use crate::import::overlay_parse::{MAX_OVERLAY_FILES, parse_overlay_zip};

/// Valid 1x1 RGBA PNG generated with exact IEEE CRC32 chunk checksums so strict decoders accept it.
fn make_tiny_png() -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
    fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let mut crc_in = Vec::with_capacity(4 + data.len());
        crc_in.extend_from_slice(tag);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }
    let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    push_chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    let raw = [0u8, 255, 0, 0, 255];
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in &raw {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    let adler = ((s2 << 16) | s1).to_be_bytes();
    let mut zlib = vec![0x78u8, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF];
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler);
    push_chunk(&mut out, b"IDAT", &zlib);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

const TINY_WEBM: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81, 0x01, 0x42, 0xF7, 0x81, 0x01];

/// Builds a valid minimal TrueType font binary declaring `family_name` in its `name` table so
/// `fontdb` / `cosmic-text` parses and accepts it cleanly in tests without host font files.
pub fn make_test_ttf(family_name: &str) -> Vec<u8> {
    let mut os2 = vec![0u8; 96];
    os2[0..2].copy_from_slice(&4u16.to_be_bytes());
    os2[4..6].copy_from_slice(&400u16.to_be_bytes()); // usWeightClass Regular
    os2[6..8].copy_from_slice(&5u16.to_be_bytes()); // usWidthClass Medium
    os2[62..64].copy_from_slice(&0x0040u16.to_be_bytes()); // fsSelection Regular
    os2[64..66].copy_from_slice(&0x0020u16.to_be_bytes()); // usFirstCharIndex
    os2[66..68].copy_from_slice(&0x0020u16.to_be_bytes()); // usLastCharIndex
    os2[68..70].copy_from_slice(&800i16.to_be_bytes()); // sTypoAscender
    os2[70..72].copy_from_slice(&(-200i16).to_be_bytes()); // sTypoDescender
    os2[74..76].copy_from_slice(&800u16.to_be_bytes()); // usWinAscent
    os2[76..78].copy_from_slice(&200u16.to_be_bytes()); // usWinDescent

    let mut cmap = Vec::with_capacity(44);
    cmap.extend_from_slice(&0u16.to_be_bytes()); // version
    cmap.extend_from_slice(&1u16.to_be_bytes()); // numTables
    cmap.extend_from_slice(&3u16.to_be_bytes()); // platformID Windows
    cmap.extend_from_slice(&1u16.to_be_bytes()); // encodingID Unicode BMP
    cmap.extend_from_slice(&12u32.to_be_bytes()); // subtable offset
    cmap.extend_from_slice(&4u16.to_be_bytes()); // format 4
    cmap.extend_from_slice(&32u16.to_be_bytes()); // length
    cmap.extend_from_slice(&0u16.to_be_bytes()); // language
    cmap.extend_from_slice(&4u16.to_be_bytes()); // segCountX2 (2 segments)
    cmap.extend_from_slice(&4u16.to_be_bytes()); // searchRange
    cmap.extend_from_slice(&1u16.to_be_bytes()); // entrySelector
    cmap.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
    cmap.extend_from_slice(&0x0020u16.to_be_bytes()); // endCount[0]
    cmap.extend_from_slice(&0xFFFFu16.to_be_bytes()); // endCount[1]
    cmap.extend_from_slice(&0u16.to_be_bytes()); // reservedPad
    cmap.extend_from_slice(&0x0020u16.to_be_bytes()); // startCount[0]
    cmap.extend_from_slice(&0xFFFFu16.to_be_bytes()); // startCount[1]
    cmap.extend_from_slice(&(-32i16).to_be_bytes()); // idDelta[0]
    cmap.extend_from_slice(&1i16.to_be_bytes()); // idDelta[1]
    cmap.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[0]
    cmap.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset[1]

    let glyf = vec![0u8; 4];

    let mut head = vec![0u8; 54];
    head[0..4].copy_from_slice(&[0, 1, 0, 0]);
    head[12..16].copy_from_slice(&0x5F0F3CF5u32.to_be_bytes());
    head[16..18].copy_from_slice(&0x0003u16.to_be_bytes());
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head[40..42].copy_from_slice(&500i16.to_be_bytes()); // xMax
    head[42..44].copy_from_slice(&800i16.to_be_bytes()); // yMax
    head[46..48].copy_from_slice(&8u16.to_be_bytes()); // lowestRecPPEM
    head[50..52].copy_from_slice(&0i16.to_be_bytes()); // short loca

    let mut hhea = vec![0u8; 36];
    hhea[0..4].copy_from_slice(&[0, 1, 0, 0]);
    hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
    hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
    hhea[10..12].copy_from_slice(&500u16.to_be_bytes()); // advanceWidthMax
    hhea[34..36].copy_from_slice(&1u16.to_be_bytes()); // numberOfHMetrics

    let mut hmtx = vec![0u8; 4];
    hmtx[0..2].copy_from_slice(&500u16.to_be_bytes());

    let loca = vec![0u8; 4];

    let mut maxp = vec![0u8; 32];
    maxp[0..4].copy_from_slice(&0x00010000u32.to_be_bytes()); // TrueType v1.0
    maxp[4..6].copy_from_slice(&1u16.to_be_bytes());

    let enc16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect() };
    let family_u16 = enc16(family_name);
    let sub_u16 = enc16("Regular");
    let ps_name: String = family_name.chars().filter(|c| !c.is_whitespace()).collect();
    let ps_u16 = enc16(if ps_name.is_empty() { "TestFont" } else { &ps_name });
    let records: [(u16, &[u8]); 4] = [(1, &family_u16), (2, &sub_u16), (4, &family_u16), (6, &ps_u16)];
    let mut name = Vec::new();
    name.extend_from_slice(&0u16.to_be_bytes());
    name.extend_from_slice(&(records.len() as u16).to_be_bytes());
    name.extend_from_slice(&(6u16 + records.len() as u16 * 12).to_be_bytes());
    let mut str_off = 0u16;
    for &(name_id, bytes) in &records {
        name.extend_from_slice(&3u16.to_be_bytes()); // Windows platform
        name.extend_from_slice(&1u16.to_be_bytes()); // Unicode BMP
        name.extend_from_slice(&0x0409u16.to_be_bytes()); // en-US
        name.extend_from_slice(&name_id.to_be_bytes());
        name.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        name.extend_from_slice(&str_off.to_be_bytes());
        str_off += bytes.len() as u16;
    }
    for &(_, bytes) in &records {
        name.extend_from_slice(bytes);
    }

    let mut post = vec![0u8; 32];
    post[0..4].copy_from_slice(&0x00030000u32.to_be_bytes()); // v3.0 (no glyph names)

    // Strictly ASCII-sorted table tags required by TrueType / OpenType / ttf-parser.
    let tables: [(&[u8; 4], Vec<u8>); 10] = [
        (b"OS/2", os2),
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
        (b"name", name),
        (b"post", post),
    ];

    let num_tables = tables.len() as u16;
    let header_len = 12 + tables.len() * 16;
    let mut offset = header_len;
    let mut out = Vec::new();
    out.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
    out.extend_from_slice(&num_tables.to_be_bytes());
    out.extend_from_slice(&128u16.to_be_bytes()); // searchRange
    out.extend_from_slice(&3u16.to_be_bytes()); // entrySelector
    out.extend_from_slice(&32u16.to_be_bytes()); // rangeShift

    let mut padded = Vec::new();
    for (tag, mut data) in tables {
        let orig_len = data.len() as u32;
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        let mut sum = 0u32;
        for chunk in data.as_chunks::<4>().0 {
            sum = sum.wrapping_add(u32::from_be_bytes(*chunk));
        }
        out.extend_from_slice(tag);
        out.extend_from_slice(&sum.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&orig_len.to_be_bytes());
        offset += data.len();
        padded.extend_from_slice(&data);
    }
    out.extend_from_slice(&padded);
    out
}

fn build_zip(entries: Vec<(&str, &[u8], Option<u32>)>) -> Vec<u8> {
    let buf = Cursor::new(Vec::new());
    let mut zw = ZipWriter::new(buf);
    for (name, data, mode) in entries {
        let mut opts = SimpleFileOptions::default();
        if let Some(m) = mode {
            opts = opts.unix_permissions(m);
        }
        zw.start_file(name, opts).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

fn streamlabs_sample_json(extra_items: Vec<Value>) -> Value {
    let mut items = vec![
        json!({
            "id": "it-bg",
            "name": "Background Loop",
            "sceneNodeType": "item",
            "x": 0.0,
            "y": 0.0,
            "scaleX": 1.0 / 1920.0,
            "scaleY": 1.0 / 1080.0,
            "visible": true,
            "locked": false,
            "content": { "nodeType": "VideoNode", "data": { "filename": "assets/loop.webm", "settings": { "looping": true } } }
        }),
        json!({
            "id": "it-frame",
            "name": "Cam Border",
            "sceneNodeType": "item",
            "x": 0.1,
            "y": 0.2,
            "scaleX": 0.5 / 1920.0,
            "scaleY": 0.5 / 1080.0,
            "visible": true,
            "locked": true,
            "content": { "nodeType": "ImageNode", "data": { "filename": "assets/frame.png" } }
        }),
        json!({
            "id": "it-title",
            "name": "Header Text",
            "sceneNodeType": "item",
            "x": 0.05,
            "y": 0.05,
            "scaleX": 1.0 / 1920.0,
            "scaleY": 1.0 / 1080.0,
            "visible": true,
            "locked": false,
            "content": {
                "nodeType": "TextNode",
                "data": {
                    "settings": {
                        "text": "LIVE NOW",
                        "font": { "face": "FerroMockFont", "size": 52, "flags": 1 },
                        "color": 0xFFFFFFFFu32
                    }
                }
            }
        }),
        json!({
            "id": "it-alerts",
            "name": "Main Alerts",
            "sceneNodeType": "item",
            "x": 0.25,
            "y": 0.1,
            "scaleX": 1.0 / 1920.0,
            "scaleY": 1.0 / 1080.0,
            "visible": true,
            "locked": false,
            "content": { "nodeType": "WidgetNode", "data": { "type": 0, "settings": {} } }
        }),
        json!({
            "id": "it-label",
            "name": "Latest Follower",
            "sceneNodeType": "item",
            "x": 0.04,
            "y": 0.9,
            "scaleX": 1.0 / 1920.0,
            "scaleY": 1.0 / 1080.0,
            "visible": true,
            "locked": false,
            "content": {
                "nodeType": "StreamlabelNode",
                "data": {
                    "labelType": "most_recent_follower",
                    "textSource": { "nodeType": "TextNode", "data": { "settings": { "custom_font": "FerroMockFont-Bold.ttf" } } }
                }
            }
        }),
        json!({
            "id": "it-unknown-widget",
            "name": "Legacy Sponsor",
            "sceneNodeType": "item",
            "x": 0.7,
            "y": 0.8,
            "scaleX": 1.0 / 1920.0,
            "scaleY": 1.0 / 1080.0,
            "visible": true,
            "locked": false,
            "content": { "nodeType": "WidgetNode", "data": { "type": "SponsorBanner", "settings": {} } }
        }),
    ];
    items.extend(extra_items);
    json!({
        "schemaVersion": 2,
        "nodeType": "RootNode",
        "data": {
            "transition": { "nodeType": "TransitionNode", "data": { "type": "obs_stinger_transition", "duration": 650, "path": "assets/stinger.webm" } },
            "scenes": {
                "data": {
                    "items": [
                        { "name": "Live Scene", "sceneId": "sc-1", "slots": { "data": { "items": items } } },
                        { "name": "BRB", "sceneId": "sc-2", "slots": { "data": { "items": [
                            { "id": "it-cam", "name": "Webcam", "sceneNodeType": "item", "x": 0.5, "y": 0.5, "scaleX": 1.0 / 1920.0, "scaleY": 1.0 / 1080.0, "content": { "nodeType": "WebcamNode", "data": { "width": 0.4, "height": 0.4 } } }
                        ] } } }
                    ]
                }
            }
        }
    })
}

struct MockFontServer {
    base: String,
    css_hits: Arc<AtomicUsize>,
    ttf_hits: Arc<AtomicUsize>,
    other_host_hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
    other_handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockFontServer {
    fn drop(&mut self) {
        self.handle.abort();
        self.other_handle.abort();
    }
}

async fn start_mock_google_fonts(mode: &'static str) -> MockFontServer {
    let other_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_addr = other_listener.local_addr().unwrap();
    let other_hits = Arc::new(AtomicUsize::new(0));
    let other_hits_c = other_hits.clone();
    let other_handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = other_listener.accept().await {
            other_hits_c.fetch_add(1, Ordering::Relaxed);
            let mut tmp = [0u8; 1024];
            let _ = sock.read(&mut tmp).await;
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nevil").await;
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let css_hits = Arc::new(AtomicUsize::new(0));
    let ttf_hits = Arc::new(AtomicUsize::new(0));
    let css_hits_c = css_hits.clone();
    let ttf_hits_c = ttf_hits.clone();
    let base_c = base.clone();
    let font_bytes = Arc::new(make_test_ttf("FerroMockFont"));

    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut req_buf = [0u8; 4096];
            let n = sock.read(&mut req_buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("/");

            if path.starts_with("/css2") {
                css_hits_c.fetch_add(1, Ordering::Relaxed);
                if mode == "404" || path.contains("UnknownCommercialFont") {
                    let body = b"/* missing */";
                    let hdr = format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                } else if mode == "ssrf" {
                    let css = format!("@font-face {{ src: url(http://{other_addr}/steal.ttf) format('truetype'); }}");
                    let hdr =
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\n\r\n", css.len());
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(css.as_bytes()).await;
                } else {
                    let css = format!(
                        "@font-face {{ font-family: 'FerroMockFont'; src: url({base_c}/font.ttf) format('truetype'); }}"
                    );
                    let hdr =
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\n\r\n", css.len());
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(css.as_bytes()).await;
                }
            } else if path.starts_with("/font.ttf") {
                ttf_hits_c.fetch_add(1, Ordering::Relaxed);
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: font/ttf\r\nContent-Length: {}\r\n\r\n",
                    font_bytes.len()
                );
                let _ = sock.write_all(hdr.as_bytes()).await;
                let _ = sock.write_all(&font_bytes).await;
            } else {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            }
        }
    });

    MockFontServer { base, css_hits, ttf_hits, other_host_hits: other_hits, handle, other_handle }
}

async fn engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::start_with_memory_secrets(dir.path()).await.unwrap();
    (e, dir)
}

#[test]
fn rejects_zip_slip_symlinks_disallowed_files_broken_json_and_size_caps() {
    let valid_cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();

    let tiny_png = make_tiny_png();
    // 1. Zip-slip traversal & Windows paths & NUL
    for bad_path in ["../evil.png", "assets/../../etc/passwd.json", "/abs/file.png", "win\\file.png", "C:drive.png"] {
        let zip = build_zip(vec![("config.json", &valid_cfg, None), (bad_path, &tiny_png, None)]);
        let err = parse_overlay_zip(&zip, "bad.overlay").unwrap_err();
        assert!(
            matches!(err, EngineError::Refused { status: 415, code: "unsafe_overlay_file", .. }),
            "{bad_path}: {err:?}"
        );
    }

    // 2. Symbolic link inside archive
    let sym_buf = Cursor::new(Vec::new());
    let mut sym_zw = ZipWriter::new(sym_buf);
    sym_zw.start_file("config.json", SimpleFileOptions::default()).unwrap();
    sym_zw.write_all(&valid_cfg).unwrap();
    sym_zw.add_symlink("assets/link.png", "/etc/passwd", SimpleFileOptions::default()).unwrap();
    let sym_zip = sym_zw.finish().unwrap().into_inner();
    assert!(matches!(
        parse_overlay_zip(&sym_zip, "sym.overlay"),
        Err(EngineError::Refused { status: 415, code: "unsafe_overlay_file", .. })
    ));

    // 3. Disallowed extensions (.js, .html, .svg, .exe)
    for ext_file in ["evil.js", "index.html", "logo.svg", "run.exe"] {
        let zip = build_zip(vec![("config.json", &valid_cfg, None), (ext_file, b"alert(1)", None)]);
        assert!(
            matches!(
                parse_overlay_zip(&zip, "ext.overlay"),
                Err(EngineError::Refused { status: 415, code: "unsupported_type", .. })
            ),
            "{ext_file}"
        );
    }

    // 4. MIME/magic spoofing (HTML or EXE disguised as .png)
    let spoofed_zip = build_zip(vec![
        ("config.json", &valid_cfg, None),
        ("assets/frame.png", b"<html><script>x</script></html>", None),
    ]);
    assert!(matches!(
        parse_overlay_zip(&spoofed_zip, "spoof.overlay"),
        Err(EngineError::Refused { status: 415, code: "unsupported_type", .. })
    ));

    // 5. Broken JSON
    let broken_zip = build_zip(vec![("config.json", b"{ broken json", None)]);
    assert!(matches!(
        parse_overlay_zip(&broken_zip, "broken.overlay"),
        Err(EngineError::Refused { status: 422, code: "invalid_overlay", .. })
    ));

    // 6. Entry count cap (> MAX_OVERLAY_FILES)
    let mut many_entries: Vec<(String, Vec<u8>)> = vec![("config.json".into(), valid_cfg)];
    for i in 0..MAX_OVERLAY_FILES {
        many_entries.push((format!("assets/p{i}.png"), tiny_png.clone()));
    }
    let refs: Vec<(&str, &[u8], Option<u32>)> =
        many_entries.iter().map(|(n, b)| (n.as_str(), b.as_slice(), None)).collect();
    let many_zip = build_zip(refs);
    assert!(matches!(
        parse_overlay_zip(&many_zip, "huge.overlay"),
        Err(EngineError::Refused { status: 413, code: "too_large", .. })
    ));
}

#[test]
fn parses_own3d_and_reports_missing_assets_and_unknown_sources_without_failing() {
    let obs_doc = json!({
        "name": "OWN3D Dark Pack",
        "current_scene": "Starting",
        "scene_order": [{ "name": "Starting" }],
        "transitions": [{ "name": "Stinger", "id": "obs_stinger_transition", "settings": { "path": "assets/stinger.webm" } }],
        "sources": [
            {
                "name": "Starting",
                "id": "scene",
                "settings": { "items": [{ "id": 1, "name": "BG" }, { "id": 2, "name": "Title" }, { "id": 3, "name": "Mystery" }] }
            },
            { "name": "BG", "id": "image_source", "settings": { "file": "assets/missing-bg.png" } },
            { "name": "Title", "id": "text_ft2_source_v2", "settings": { "text": "SOON", "font": { "face": "Orbitron", "size": 60 } } },
            { "name": "Mystery", "id": "thirdparty_magic_source", "settings": {} }
        ],
        "widgets": [
            { "scene": "Starting", "name": "Alerts", "type": "alertbox", "x": 100, "y": 50 },
            { "scene": "Starting", "name": "Weird", "type": "custom_sponsor_thing" }
        ]
    });
    let manifest = json!({ "name": "OWN3D Cyber", "brand": "own3d", "fonts": ["Orbitron"] });
    let zip = build_zip(vec![
        ("manifest.json", serde_json::to_vec(&manifest).unwrap().as_slice(), None),
        ("scenes.json", serde_json::to_vec(&obs_doc).unwrap().as_slice(), None),
        ("assets/stinger.webm", TINY_WEBM, None),
    ]);
    let parsed = parse_overlay_zip(&zip, "cyber.overlay").unwrap();
    assert_eq!(parsed.format, OverlayPackFormat::Own3d);
    assert!(parsed.missing_assets.iter().any(|a| a.contains("missing-bg.png")));
    assert!(parsed.widgets.iter().any(|w| w.supported && w.mapped_to == Some(WidgetKind::Alerts)));
    assert!(parsed.unknown_sources.iter().any(|u| u.kind == "custom_sponsor_thing"));
    assert!(parsed.missing_kinds.iter().any(|k| k == "thirdparty_magic_source"));
    assert!(parsed.font_specs.iter().any(|f| f.family == "Orbitron"));
}

#[tokio::test]
async fn preview_import_mocked_google_fonts_caching_and_undo_lifecycle() {
    let mock = start_mock_google_fonts("ok").await;
    let (e, dir) = engine().await;
    e.set_google_fonts_api(&mock.base);

    let tiny_png = make_tiny_png();
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let zip = build_zip(vec![
        ("config.json", &cfg, None),
        ("assets/frame.png", &tiny_png, None),
        ("assets/loop.webm", TINY_WEBM, None),
        ("assets/stinger.webm", TINY_WEBM, None),
    ]);

    // 1. Preview without mutating disk or hitting network
    let preview = e.preview_overlay_pack(zip.clone(), "NeonTheme.overlay".into()).await.unwrap();
    assert_eq!(preview.format, OverlayPackFormat::Streamlabs);
    assert_eq!(preview.scene_count, 2);
    assert_eq!(preview.media_files, 3);
    assert!(preview.widgets.iter().any(|w| w.mapped_to == Some(WidgetKind::Alerts)));
    assert!(preview.widgets.iter().any(|w| w.mapped_to == Some(WidgetKind::Label)));
    assert!(preview.unknown_sources.iter().any(|u| u.kind == "SponsorBanner"));
    assert!(preview.fonts.iter().any(|f| f.family == "FerroMockFont" && f.status == FontStatus::Missing));
    assert_eq!(mock.css_hits.load(Ordering::Relaxed), 0);

    // 2. Apply import -> downloads font into <data>/fonts/, copies media, creates overlay, switches collection
    let report = e
        .import_overlay_pack(OverlayPackImportReq {
            preview_id: preview.preview_id,
            name: Some("Neon Pack".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(report.name, "Neon Pack");
    assert_eq!(report.media_copied, 3);
    assert!(!report.overlays_created.is_empty());
    assert!(!report.offline_font_fallback);
    let font_entry = report.fonts.iter().find(|f| f.family == "FerroMockFont").unwrap();
    assert_eq!(font_entry.status, FontStatus::Downloaded);
    let cached_filename = font_entry.file.as_ref().unwrap();
    assert!(fonts_dir(dir.path()).join(cached_filename).is_file(), "font stored only in <data>/fonts/");
    assert_eq!(mock.css_hits.load(Ordering::Relaxed), 1);
    assert_eq!(mock.ttf_hits.load(Ordering::Relaxed), 1);

    let lib = studio_library::load(dir.path());
    assert_eq!(lib.collections.current, report.id);
    let col_file = studio_library::file(dir.path(), studio_library::Kind::Collections, &report.id);
    assert!(col_file.is_file());
    let saved_col: Value = serde_json::from_slice(&std::fs::read(&col_file).unwrap()).unwrap();
    let img_src = saved_col["sources"].as_array().unwrap().iter().find(|s| s["name"] == "Cam Border").unwrap();
    let copied_path = Path::new(img_src["settings"]["file"].as_str().unwrap());
    assert!(copied_path.is_file() && copied_path.starts_with(dir.path()));

    // 3. Re-importing uses cached font with ZERO extra HTTP hits
    let preview2 = e.preview_overlay_pack(zip, "NeonTheme2.overlay".into()).await.unwrap();
    assert!(preview2.fonts.iter().any(|f| f.family == "FerroMockFont" && f.status == FontStatus::Cached));
    let report2 = e
        .import_overlay_pack(OverlayPackImportReq { preview_id: preview2.preview_id, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(report2.fonts[0].status, FontStatus::Cached);
    assert_eq!(mock.css_hits.load(Ordering::Relaxed), 1, "cache avoided network fetch");

    // 4. Undo second then first import cleanly
    let undo2 = e.undo_overlay_pack(&report2.undo_token).await.unwrap();
    assert_eq!(undo2.restored_collection_id.as_deref(), Some(report.id.as_str()));
    assert!(!copied_path.parent().unwrap().join("nonexistent").exists());

    let undo1 = e.undo_overlay_pack(&report.undo_token).await.unwrap();
    assert_eq!(undo1.restored_collection_id.as_deref(), Some("untitled"));
    assert_eq!(studio_library::load(dir.path()).collections.current, "untitled");
    assert!(!col_file.exists());
    assert!(!copied_path.exists(), "copied pack media cleaned up on undo");
    assert!(fonts_dir(dir.path()).join(cached_filename).is_file(), "font cache kept reusable");
    assert!(
        e.overlays().await.unwrap().is_empty()
            || e.overlays().await.unwrap().iter().all(|o| !report.overlays_created.iter().any(|c| c.id == o.id))
    );
}

#[tokio::test]
async fn offline_and_ssrf_and_bundled_font_fallbacks_never_fail_pack_import() {
    let (e, _dir) = engine().await;

    // 1. Offline / closed localhost port -> graceful OfflineFallback
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);
    e.set_google_fonts_api(&format!("http://127.0.0.1:{closed_port}"));

    let tiny_png = make_tiny_png();
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let zip = build_zip(vec![("config.json", &cfg, None), ("assets/frame.png", &tiny_png, None)]);
    let prev = e.preview_overlay_pack(zip, "offline.overlay".into()).await.unwrap();
    let rep = e
        .import_overlay_pack(OverlayPackImportReq { preview_id: prev.preview_id, ..Default::default() })
        .await
        .unwrap();
    assert!(rep.offline_font_fallback);
    assert_eq!(rep.fonts[0].status, FontStatus::OfflineFallback);
    assert!(rep.fonts[0].message.as_deref().unwrap().to_ascii_lowercase().contains("offline"));

    // 2. SSRF redirect/link pointing off Google Fonts host is blocked
    let ssrf_mock = start_mock_google_fonts("ssrf").await;
    e.set_google_fonts_api(&ssrf_mock.base);
    let zip2 = build_zip(vec![("config.json", &cfg, None)]);
    let prev2 = e.preview_overlay_pack(zip2, "ssrf.overlay".into()).await.unwrap();
    let rep2 = e
        .import_overlay_pack(OverlayPackImportReq { preview_id: prev2.preview_id, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(ssrf_mock.other_host_hits.load(Ordering::Relaxed), 0, "never requested non-allowlisted font host");
    assert_eq!(rep2.fonts[0].status, FontStatus::NotFound);

    // 3. Bundled font inside the .overlay zip skips HTTP lookup cleanly
    let bundled_ttf = make_test_ttf("FerroMockFont");
    let mock_hit_check = start_mock_google_fonts("ok").await;
    e.set_google_fonts_api(&mock_hit_check.base);
    let (e3, _d3) = engine().await;
    e3.set_google_fonts_api(&mock_hit_check.base);
    let zip3 = build_zip(vec![("config.json", &cfg, None), ("fonts/FerroMockFont-Bold.ttf", &bundled_ttf, None)]);
    let prev3 = e3.preview_overlay_pack(zip3, "bundled.overlay".into()).await.unwrap();
    assert_eq!(prev3.fonts[0].status, FontStatus::Bundled);
    let rep3 = e3
        .import_overlay_pack(OverlayPackImportReq { preview_id: prev3.preview_id, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(rep3.fonts[0].status, FontStatus::Bundled);
    assert_eq!(mock_hit_check.css_hits.load(Ordering::Relaxed), 0);
}

// ---- Neon review C1/C2: an imported pack's JSON is untrusted ---------------------------------

/// A pack of OBS JSON with one scene holding `sources` (one item each) and the given assets.
fn obs_pack(sources: Vec<Value>, assets: Vec<(&str, &[u8])>) -> ParsedPack {
    let items: Vec<Value> =
        sources.iter().enumerate().map(|(i, s)| json!({ "id": i + 1, "name": s["name"], "visible": true })).collect();
    let mut all = vec![json!({ "name": "Main", "id": "scene", "settings": { "items": items } })];
    all.extend(sources);
    let doc = json!({
        "name": "Evil",
        "current_scene": "Main",
        "scene_order": [{ "name": "Main" }],
        "transitions": [{ "name": "Fade", "id": "fade_transition", "settings": {} }],
        "sources": all,
    });
    let json_bytes = serde_json::to_vec(&doc).unwrap();
    let mut entries: Vec<(&str, &[u8], Option<u32>)> = vec![("scenes.json", &json_bytes, None)];
    for (n, d) in &assets {
        entries.push((n, d, None));
    }
    parse_overlay_zip(&build_zip(entries), "evil.overlay").unwrap()
}

fn source<'a>(p: &'a ParsedPack, name: &str) -> Option<&'a Value> {
    p.doc_template["sources"].as_array()?.iter().find(|s| s["name"] == name)
}

fn setting<'a>(p: &'a ParsedPack, name: &str, key: &str) -> Option<&'a Value> {
    source(p, name)?.get("settings")?.get(key)
}

#[test]
fn a_browser_source_loads_only_public_web_pages() {
    let bad = [
        "file:///etc/passwd",
        "FILE:///Users/me/keys.txt",
        "data:text/html,<script>fetch('x')</script>",
        "javascript:alert(1)",
        "ftp://x.test/a",
        "http://127.0.0.1:47820/o/secret/",
        "http://localhost:3000/",
        "http://[::1]/admin",
        "http://192.168.1.1/",
        "http://10.0.0.5/",
        "http://169.254.169.254/latest/meta-data/",
        "http://2130706433/",
        "http://0x7f.1/",
        "http://user:pw@example.com/",
        "https://metadata.google.internal/",
    ];
    let sources: Vec<Value> = bad
        .iter()
        .enumerate()
        .map(|(i, u)| json!({ "name": format!("B{i}"), "id": "browser_source", "settings": { "url": u } }))
        .chain([
            json!({ "name": "Good", "id": "browser_source", "settings": { "url": "https://example.com/widget?x=1", "width": 800, "height": 600 } }),
            json!({ "name": "Local", "id": "browser_source", "settings": { "is_local_file": true, "local_file": "/etc/passwd", "url": "" } }),
            json!({ "name": "LocalHtm", "id": "browser_source", "settings": { "url": "file:///x/Y.HTM" } }),
        ])
        .collect();
    let p = obs_pack(sources, vec![]);
    for (i, url) in bad.iter().enumerate() {
        assert_eq!(setting(&p, &format!("B{i}"), "url"), Some(&json!("")), "{url}");
    }
    assert_eq!(setting(&p, "Good", "url"), Some(&json!("https://example.com/widget?x=1")));
    assert_eq!(setting(&p, "Good", "width"), Some(&json!(800)));
    assert_eq!(setting(&p, "Local", "local_file"), None, "no local file for a browser source");
    assert_ne!(setting(&p, "Local", "is_local_file"), Some(&json!(true)));
    assert_eq!(setting(&p, "LocalHtm", "url"), Some(&json!("")));
}

#[test]
fn a_browser_sources_css_cannot_load_anything() {
    let p = obs_pack(
        vec![
            json!({ "name": "Bad", "id": "browser_source", "settings": { "url": "https://example.com/", "css": "@import url(http://evil.test/x.css); body{}" } }),
            json!({ "name": "Bad2", "id": "browser_source", "settings": { "url": "https://example.com/", "css": "body{background:url(http://evil.test/p.png)}" } }),
            json!({ "name": "Fine", "id": "browser_source", "settings": { "url": "https://example.com/", "css": "body { margin: 0; }" } }),
        ],
        vec![],
    );
    assert_eq!(setting(&p, "Bad", "css"), Some(&json!("")));
    assert_eq!(setting(&p, "Bad2", "css"), Some(&json!("")));
    assert_eq!(setting(&p, "Fine", "css"), Some(&json!("body { margin: 0; }")));
}

#[test]
fn files_outside_the_pack_are_never_left_in_a_source() {
    let png = make_tiny_png();
    let p = obs_pack(
        vec![
            json!({ "name": "Img", "id": "image_source", "settings": { "file": "/Users/victim/Desktop/private.png" } }),
            json!({ "name": "Ok", "id": "image_source", "settings": { "file": "assets/frame.png" } }),
            json!({ "name": "Slides", "id": "slideshow", "settings": { "files": [
                { "value": "/Users/victim/a.png", "hidden": false }, { "value": "assets/frame.png" } ], "slide_time": 5000 } }),
            json!({ "name": "Clip", "id": "ffmpeg_source", "settings": { "is_local_file": false, "input": "http://192.168.0.9/x.mp4",
                "ffmpeg_options": "protocol_whitelist=file", "local_file": "/etc/hosts" } }),
            json!({ "name": "Words", "id": "text_ft2_source_v2", "settings": { "text": "hi", "from_file": true,
                "text_file": "/etc/passwd", "file": "/etc/passwd" } }),
            json!({ "name": "Masked", "id": "image_source", "settings": { "file": "assets/frame.png" },
                "filters": [ { "name": "m", "id": "mask_filter", "settings": { "image_path": "/Users/victim/m.png", "opacity": 1 } } ] }),
        ],
        vec![("assets/frame.png", &png)],
    );
    let file = |n: &str, k: &str| setting(&p, n, k).and_then(Value::as_str).map(str::to_string);
    assert_eq!(file("Img", "file").unwrap_or_default(), "", "a path outside the pack is cleared");
    assert!(file("Ok", "file").unwrap().starts_with("__FERRO_PACK_ASSET__:"), "a pack file stays");
    let slides = setting(&p, "Slides", "files").and_then(Value::as_array).unwrap();
    assert_eq!(slides.len(), 1, "only pack files stay in a slideshow: {slides:?}");
    assert_eq!(setting(&p, "Slides", "slide_time"), Some(&json!(5000)));
    assert_eq!(setting(&p, "Clip", "is_local_file"), Some(&json!(true)));
    assert_eq!(file("Clip", "local_file").unwrap_or_default(), "");
    assert_eq!(setting(&p, "Clip", "input"), None);
    assert_eq!(setting(&p, "Clip", "ffmpeg_options"), None);
    assert_eq!(setting(&p, "Words", "text"), Some(&json!("hi")));
    for k in ["from_file", "text_file", "file"] {
        assert_eq!(setting(&p, "Words", k), None, "{k}");
    }
    let mask = &source(&p, "Masked").unwrap()["filters"];
    assert_eq!(mask[0]["settings"]["image_path"], json!(""));
}

#[test]
fn only_known_source_and_filter_kinds_come_in() {
    let p = obs_pack(
        vec![
            json!({ "name": "Vlc", "id": "vlc_source", "settings": { "playlist": [ { "value": "/etc/passwd" } ] } }),
            json!({ "name": "Web", "id": "ferrocast_web_text", "settings": { "url": "http://169.254.169.254/" } }),
            json!({ "name": "Magic", "id": "thirdparty_magic_source", "settings": {} }),
            json!({ "name": "Colour", "id": "color_source_v3", "settings": { "color": 4278190080u32, "width": 100, "height": 50, "evil": "x" },
                "filters": [
                    { "name": "v", "id": "ferrocast_vst", "settings": { "path": "/Library/Audio/Plug-Ins/VST3/x.vst3" } },
                    { "name": "s", "id": "ferrocast_shader", "settings": { "shader_file": "/etc/passwd" } },
                    { "name": "c", "id": "color_filter_v2", "settings": { "saturation": 0.5, "evil": { "a": 1 } } } ] }),
        ],
        vec![],
    );
    for gone in ["Vlc", "Web", "Magic"] {
        assert!(source(&p, gone).is_none(), "{gone} is left out");
    }
    let main = source(&p, "Main").unwrap();
    let items = main["settings"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "scene items for left-out sources go too: {items:?}");
    let colour = source(&p, "Colour").unwrap();
    assert_eq!(colour["settings"]["width"], json!(100));
    assert_eq!(colour["settings"].get("evil"), None);
    let filters = colour["filters"].as_array().unwrap();
    assert_eq!(filters.len(), 1, "only the colour filter stays: {filters:?}");
    assert_eq!(filters[0]["id"], "color_filter_v2");
    assert_eq!(filters[0]["settings"]["saturation"], json!(0.5));
    assert_eq!(filters[0]["settings"].get("evil"), None);
    assert!(p.notes.iter().any(|n| n.contains("vlc_source")), "said in the notes: {:?}", p.notes);
    assert!(p.missing_kinds.iter().any(|k| k == "thirdparty_magic_source"));
}

#[test]
fn a_streamlabs_packs_filters_and_node_settings_are_checked_too() {
    let png = make_tiny_png();
    let mut cfg = streamlabs_sample_json(vec![]);
    let items = cfg.pointer_mut("/data/scenes/data/items/0/slots/data/items").and_then(Value::as_array_mut).unwrap();
    // The text node reads a file and has a helper-spawning filter; the image node a stray path.
    for it in items.iter_mut() {
        match it["id"].as_str() {
            Some("it-title") => {
                it["content"]["data"]["settings"]["text_file"] = json!("/etc/passwd");
                it["content"]["data"]["settings"]["from_file"] = json!(true);
                it["filters"] = json!([
                    { "type": "ferrocast_vst", "name": "v", "settings": { "path": "/x.vst3" } },
                    { "type": "color_filter_v2", "name": "c", "settings": { "saturation": 0.4 } }
                ]);
            }
            Some("it-frame") => {
                it["content"]["data"]["settings"] = json!({ "unload": true, "bogus": [1, 2] });
            }
            _ => {}
        }
    }
    let text = serde_json::to_vec(&cfg).unwrap();
    let zip = build_zip(vec![
        ("config.json", &text, None),
        ("assets/frame.png", &png, None),
        ("assets/loop.webm", TINY_WEBM, None),
        ("assets/stinger.webm", TINY_WEBM, None),
    ]);
    let p = parse_overlay_zip(&zip, "x.overlay").unwrap();
    let all = serde_json::to_string(&p.doc_template).unwrap();
    assert!(!all.contains("ferrocast_vst"), "no helper-spawning filter survives: {all}");
    assert!(!all.contains("/etc/passwd") && !all.contains("text_file"), "no file read left: {all}");
    let title = source(&p, "Header Text").unwrap();
    assert_eq!(title["filters"].as_array().map(Vec::len), Some(1));
    assert_eq!(title["filters"][0]["id"], "color_filter_v2");
    assert_eq!(setting(&p, "Header Text", "text"), Some(&json!("LIVE NOW")));
    assert_eq!(setting(&p, "Cam Border", "bogus"), None);
    assert_eq!(setting(&p, "Cam Border", "unload"), Some(&json!(true)));
}

#[test]
fn a_streamlabs_web_source_on_this_computer_or_network_is_emptied_and_notes_hide_secrets() {
    let png = make_tiny_png();
    let web = |id: &str, name: &str, url: &str| {
        json!({
            "id": id, "name": name, "sceneNodeType": "item", "x": 0.1, "y": 0.1,
            "scaleX": 1.0 / 1920.0, "scaleY": 1.0 / 1080.0, "visible": true, "locked": false,
            "content": { "nodeType": "SmartBrowserNode", "data": { "settings": { "url": url, "width": 640, "height": 360 } } }
        })
    };
    let cfg = streamlabs_sample_json(vec![
        web("w1", "Metadata", "http://169.254.169.254/latest/meta-data/"),
        web("w2", "Secret", "https://user:hunter2@evil.test/path#access_token=abc123"),
        web("w3", "Admin", "http://192.168.1.1/admin"),
        web("w4", "Public", "https://widgets.example.com/chat?token=zzz"),
    ]);
    let cfg = serde_json::to_vec(&cfg).unwrap();
    let zip = build_zip(vec![
        ("config.json", &cfg, None),
        ("assets/frame.png", &png, None),
        ("assets/loop.webm", TINY_WEBM, None),
        ("assets/stinger.webm", TINY_WEBM, None),
    ]);
    let p = parse_overlay_zip(&zip, "x.overlay").unwrap();
    for n in ["Metadata", "Secret", "Admin"] {
        assert!(source(&p, n).is_none(), "{n} isn't imported");
        assert!(p.unknown_sources.iter().any(|u| u.name == n), "{n} is reported");
    }
    assert_eq!(setting(&p, "Public", "url"), Some(&json!("https://widgets.example.com/chat?token=zzz")));
    let notes = p.notes.join("\n");
    for secret in ["hunter2", "access_token", "abc123", "zzz"] {
        assert!(!notes.contains(secret), "{secret} in the notes: {notes}");
    }
}

// ---- Neon review C2: placeholders are filled only where a pack's own file or overlay goes ------

#[test]
fn the_overlay_token_and_paths_never_land_in_text_a_pack_controls() {
    let secret_url = "http://127.0.0.1:47820/o/SECRET-TOKEN/".to_string();
    let urls: BTreeMap<String, String> = BTreeMap::from([("__FERRO_OVERLAY_URL__:0".to_string(), secret_url.clone())]);
    let paths = vec!["/Users/victim/Library/Application Support/Ferrocast/studio/packs/p1/assets/a.png".to_string()];
    let tmpl = json!({
        "name": "__FERRO_PACK_ASSET__:0",
        "sources": [
            { "name": "__FERRO_OVERLAY_URL__:0", "id": "text_ft2_source_v2",
              "settings": { "text": "__FERRO_OVERLAY_URL__:0", "extra": "__FERRO_PACK_ASSET__:0" } },
            { "name": "Pic", "id": "image_source", "settings": { "file": "__FERRO_PACK_ASSET__:0" },
              "filters": [ { "id": "mask_filter", "settings": { "image_path": "__FERRO_PACK_ASSET__:0" } } ] },
            { "name": "Slides", "id": "slideshow", "settings": { "files": [ { "value": "__FERRO_PACK_ASSET__:0" } ] } },
            { "name": "Overlay", "id": "ferrocast_web", "settings": { "url": "__FERRO_OVERLAY_URL__:0" } }
        ],
        "transitions": [ { "id": "obs_stinger_transition", "settings": { "path": "__FERRO_PACK_ASSET__:0" } } ]
    });
    let out = finalize_doc(&tmpl, &paths, &urls);
    // Where a file or the overlay's address belongs, the real value goes in.
    assert_eq!(out["sources"][1]["settings"]["file"], json!(paths[0]));
    assert_eq!(out["sources"][1]["filters"][0]["settings"]["image_path"], json!(paths[0]));
    assert_eq!(out["sources"][2]["settings"]["files"][0]["value"], json!(paths[0]));
    assert_eq!(out["sources"][3]["settings"]["url"], json!(secret_url));
    assert_eq!(out["transitions"][0]["settings"]["path"], json!(paths[0]));
    // Anywhere else the words stay words: nothing a viewer could read.
    let text_source = serde_json::to_string(&out["sources"][0]).unwrap();
    assert!(!text_source.contains("SECRET-TOKEN") && !text_source.contains("/Users/victim"), "{text_source}");
    assert!(!out["name"].to_string().contains("/Users/victim"));
}

// ---- Neon review H1: a pack's fonts can't take over fonts already on this computer ------------

fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out
}

async fn import_pack(e: &Engine, zip: Vec<u8>, name: &str) -> OverlayPackImportReport {
    let prev = e.preview_overlay_pack(zip, name.into()).await.unwrap();
    e.import_overlay_pack(OverlayPackImportReq { preview_id: prev.preview_id, ..Default::default() }).await.unwrap()
}

#[tokio::test]
async fn a_bundled_font_lives_with_its_pack_and_leaves_with_it() {
    let (e, dir) = engine().await;
    let mock = start_mock_google_fonts("ok").await;
    e.set_google_fonts_api(&mock.base);
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let ttf = make_test_ttf("FerroMockFont");
    let rep = import_pack(
        &e,
        build_zip(vec![("config.json", &cfg, None), ("fonts/FerroMockFont-Bold.ttf", &ttf, None)]),
        "a.overlay",
    )
    .await;
    assert_eq!(rep.fonts[0].status, FontStatus::Bundled);
    let fonts = fonts_dir(dir.path());
    let under_packs = files_under(&fonts.join("packs"));
    assert_eq!(under_packs.len(), 1, "the font sits in the pack's own folder: {under_packs:?}");
    let index = std::fs::read_to_string(fonts.join("index.json")).unwrap_or_default();
    assert!(!index.contains("bundled"), "the shared font cache isn't touched: {index}");
    e.undo_overlay_pack(&rep.undo_token).await.unwrap();
    assert!(files_under(&fonts.join("packs")).is_empty(), "the pack's font goes on undo");
}

#[tokio::test]
async fn a_bundled_font_never_replaces_a_family_already_cached() {
    let (e, dir) = engine().await;
    let mock = start_mock_google_fonts("ok").await;
    e.set_google_fonts_api(&mock.base);
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    // The family is fetched and cached first (what a later pack's font would want to replace).
    let first = import_pack(&e, build_zip(vec![("config.json", &cfg, None)]), "a.overlay").await;
    let _ = first;
    let before = std::fs::read_to_string(fonts_dir(dir.path()).join("index.json")).unwrap();
    assert!(before.contains("google_fonts"), "the family is cached from Google: {before}");
    let mut evil = make_test_ttf("FerroMockFont");
    evil.extend_from_slice(b"trojan"); // a different file, same family
    let rep =
        import_pack(&e, build_zip(vec![("config.json", &cfg, None), ("fonts/Evil.ttf", &evil, None)]), "b.overlay")
            .await;
    assert_eq!(rep.fonts[0].status, FontStatus::Cached, "the cached font is the one used");
    assert_eq!(std::fs::read_to_string(fonts_dir(dir.path()).join("index.json")).unwrap(), before);
    assert!(files_under(&fonts_dir(dir.path()).join("packs")).is_empty(), "the pack's copy isn't installed");
}

#[tokio::test]
async fn a_woff_with_only_its_magic_bytes_is_not_installed() {
    let (e, dir) = engine().await;
    let mock = start_mock_google_fonts("ok").await;
    e.set_google_fonts_api(&mock.base);
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let mut fake = b"wOF2".to_vec();
    fake.extend_from_slice(&[0u8; 32]);
    let rep = import_pack(
        &e,
        build_zip(vec![("config.json", &cfg, None), ("fonts/FerroMockFont.woff2", &fake, None)]),
        "w.overlay",
    )
    .await;
    assert_ne!(rep.fonts[0].status, FontStatus::Bundled, "a woff can't be loaded, so it isn't claimed");
    let all: Vec<_> = files_under(&fonts_dir(dir.path()))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "woff2"))
        .collect();
    assert!(all.is_empty(), "{all:?}");
}

// ---- Neon review H4: font downloads are read with a cap, as they arrive ----------------------

/// A font server that streams a body of `total` bytes with no Content-Length (chunked), for
/// `what` = "css" (the stylesheet itself) or "font" (the file the stylesheet names). Counts the
/// body bytes it got to send.
async fn start_streaming_font_server(
    what: &'static str,
    total: usize,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let sent = Arc::new(AtomicUsize::new(0));
    let (sent_c, base_c) = (sent.clone(), base.clone());
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let sent = sent_c.clone();
            let base = base_c.clone();
            tokio::spawn(async move {
                let mut req = [0u8; 4096];
                let n = sock.read(&mut req).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&req[..n]).to_string();
                let path = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/").to_string();
                let stream_it =
                    (path.starts_with("/css2") && what == "css") || (path.starts_with("/big.ttf") && what == "font");
                if path.starts_with("/css2") && what == "font" {
                    let css = format!(
                        "@font-face {{ font-family: 'FerroMockFont'; src: url({base}/big.ttf) format('truetype'); }}"
                    );
                    let hdr =
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\n\r\n", css.len());
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(css.as_bytes()).await;
                } else if stream_it {
                    if sock.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").await.is_err() {
                        return;
                    }
                    let chunk = vec![b'a'; 64 * 1024];
                    let mut left = total;
                    while left > 0 {
                        let n = left.min(chunk.len());
                        let head = format!("{n:x}\r\n");
                        if sock.write_all(head.as_bytes()).await.is_err()
                            || sock.write_all(&chunk[..n]).await.is_err()
                            || sock.write_all(b"\r\n").await.is_err()
                        {
                            return;
                        }
                        sent.fetch_add(n, Ordering::Relaxed);
                        left -= n;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                }
            });
        }
    });
    (base, sent, handle)
}

#[tokio::test]
async fn an_endless_font_stylesheet_or_file_is_cut_off_as_it_arrives() {
    for (what, total, cap_hint) in
        [("css", 64 * 1024 * 1024, 4 * 1024 * 1024), ("font", 64 * 1024 * 1024, 30 * 1024 * 1024)]
    {
        let (e, dir) = engine().await;
        let (base, sent, handle) = start_streaming_font_server(what, total).await;
        e.set_google_fonts_api(&base);
        let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
        let rep = import_pack(&e, build_zip(vec![("config.json", &cfg, None)]), "big.overlay").await;
        assert!(
            !matches!(rep.fonts[0].status, FontStatus::Cached | FontStatus::Bundled | FontStatus::Downloaded),
            "{what}: {:?}",
            rep.fonts[0].status
        );
        assert!(
            files_under(&fonts_dir(dir.path())).iter().all(|p| p.extension().is_none_or(|x| x != "ttf")),
            "{what}: nothing saved"
        );
        // Give the server a moment to notice the closed connection, then see how far it got.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let got = sent.load(Ordering::Relaxed);
        assert!(got < cap_hint, "{what}: the whole {total} bytes were read ({got} sent), not cut off");
        handle.abort();
    }
}

// ---- Neon review H3: a small upload can't become a lot of memory ------------------------------

#[test]
fn a_file_that_unpacks_to_a_thousand_times_its_size_is_refused() {
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    // 4 MB of zeros packs to a few KB: a zip bomb in small.
    let zeros = vec![0u8; 4 * 1024 * 1024];
    let zip = build_zip(vec![("config.json", &cfg, None), ("assets/loop.webm", &zeros, None)]);
    assert!(zip.len() < 64 * 1024, "the bomb is small: {}", zip.len());
    match parse_overlay_zip(&zip, "bomb.overlay") {
        Err(EngineError::Refused { status: 413, code, .. }) => assert_eq!(code, "suspicious_compression"),
        other => panic!("expected a refusal for the compression ratio, got {:?}", other.map(|p| p.suggested_name)),
    }
}

#[tokio::test]
async fn only_one_pack_is_read_at_a_time_and_few_previews_are_kept() {
    let (e, _dir) = engine().await;
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let zip = build_zip(vec![("config.json", &cfg, None)]);
    {
        // While an import (or another preview) holds the lock, a preview is turned away, not queued.
        let _held = e.inner.import_busy.lock().await;
        match e.preview_overlay_pack(zip.clone(), "a.overlay".into()).await {
            Err(EngineError::Conflict(_)) => {}
            other => panic!("a preview while busy should be refused: {:?}", other.map(|p| p.preview_id)),
        }
    }
    // Only two previews are kept in memory: the oldest goes.
    let first = e.preview_overlay_pack(zip.clone(), "1.overlay".into()).await.unwrap();
    e.preview_overlay_pack(zip.clone(), "2.overlay".into()).await.unwrap();
    e.preview_overlay_pack(zip, "3.overlay".into()).await.unwrap();
    let gone = e.import_overlay_pack(OverlayPackImportReq { preview_id: first.preview_id, ..Default::default() }).await;
    assert!(matches!(gone, Err(EngineError::Refused { status: 404, .. })), "{:?}", gone.map(|r| r.id));
}

#[test]
fn what_is_left_of_the_total_limit_bounds_each_read() {
    use crate::import::overlay_parse::entry_read_limit;
    assert_eq!(entry_read_limit(60, 1_000), 60);
    assert_eq!(entry_read_limit(60, 10), 10, "near the total limit the read is cut there");
    assert_eq!(entry_read_limit(60, 0), 0);
}

// ---- Neon review H5: nothing swaps the scene collection while a show is on --------------------

fn start_show(e: &Engine) {
    e.studio().machine.lock().unwrap_or_else(|p| p.into_inner()).phase = crate::studio::show::BroadcastPhase::Live;
}

#[tokio::test]
async fn a_headless_engine_refuses_to_change_collections_during_a_show() {
    let (e, _dir) = engine().await;
    start_show(&e);
    for act in [
        ShelfAct::Switch { id: "untitled".into() },
        ShelfAct::New { name: "X".into(), copy: false },
        ShelfAct::Import { name: "Y".into(), doc: json!({ "name": "Y", "sources": [] }) },
    ] {
        match e.studio_shelf("collections", act).await {
            Err(EngineError::Refused { status: 409, code: "studio_busy", .. }) => {}
            other => panic!("expected 409 studio_busy, got {:?}", other.map(|v| v.to_string())),
        }
    }
}

/// A font server that answers the stylesheet after `delay_ms`, with "not found".
async fn start_slow_font_server(delay_ms: u64) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_c = hits.clone();
    let handle = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let hits = hits_c.clone();
            tokio::spawn(async move {
                let mut req = [0u8; 2048];
                let _ = sock.read(&mut req).await;
                hits.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            });
        }
    });
    (base, hits, handle)
}

#[tokio::test]
async fn a_show_that_starts_while_an_import_downloads_fonts_stops_the_import() {
    let (e, dir) = engine().await;
    let (base, hits, handle) = start_slow_font_server(500).await;
    e.set_google_fonts_api(&base);
    let png = make_tiny_png();
    let cfg = serde_json::to_vec(&streamlabs_sample_json(vec![])).unwrap();
    let zip = build_zip(vec![("config.json", &cfg, None), ("assets/frame.png", &png, None)]);
    let prev = e.preview_overlay_pack(zip, "slow.overlay".into()).await.unwrap();
    let before = studio_library::load(dir.path()).collections.current;
    let e2 = e.clone();
    let importing = tokio::spawn(async move {
        e2.import_overlay_pack(OverlayPackImportReq { preview_id: prev.preview_id, ..Default::default() }).await
    });
    // The import is now waiting on the font server: a show goes live.
    for _ in 0..100 {
        if hits.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(hits.load(Ordering::Relaxed) > 0, "the import reached the font download");
    start_show(&e);
    match importing.await.unwrap() {
        Err(EngineError::Refused { status: 409, code: "studio_busy", .. }) => {}
        other => panic!("expected 409 studio_busy, got {:?}", other.map(|r| r.id)),
    }
    assert_eq!(studio_library::load(dir.path()).collections.current, before, "the collection was not swapped");
    assert!(files_under(&dir.path().join("studio").join("packs")).is_empty(), "the copied media was removed");
    handle.abort();
}

// ---- Neon review M4: no half-made overlay source when the overlay wasn't made -----------------

#[test]
fn a_source_for_an_overlay_that_was_not_made_is_left_out_with_its_scene_item() {
    let tmpl = json!({
        "name": "P",
        "sources": [
            { "name": "Main", "id": "scene", "settings": { "items": [
                { "id": 1, "name": "Title" }, { "id": 2, "name": "Main Overlay" } ] } },
            { "name": "Title", "id": "text_ft2_source_v2", "settings": { "text": "hi" } },
            { "name": "Main Overlay", "id": "ferrocast_web", "settings": { "url": "__FERRO_OVERLAY_URL__:0", "width": 1920 } }
        ]
    });
    // create_overlays was off (or making it failed): nothing in the map.
    let out = finalize_doc(&tmpl, &[], &BTreeMap::new());
    let text = out.to_string();
    assert!(!text.contains("__FERRO_OVERLAY_URL__"), "{text}");
    let names: Vec<&str> = out["sources"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["Main", "Title"]);
    assert_eq!(out["sources"][0]["settings"]["items"].as_array().unwrap().len(), 1);
}
