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
