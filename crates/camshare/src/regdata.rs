//! What registering the camera writes, as data: keys under a `Software\Classes` root (the
//! user's, HKCU, for DirectShow; the machine's, HKLM, for Media Foundation's frame server).
//! Written by the app (DirectShow, no admin) and by the DLL's `DllRegisterServer` (elevated).

use crate::{CLSID_DSHOW, CLSID_MF, FRIENDLY_NAME, guid_bytes, guid_text};

/// DirectShow's "Video Capture Sources" category: cameras are listed under it.
pub const CLSID_VIDEO_INPUT_CATEGORY: u128 = 0x860B_B310_5D01_11D0_BD3B_00A0_C911_CE86;
/// PIN_CATEGORY_CAPTURE.
pub const PIN_CATEGORY_CAPTURE: u128 = 0xFB6C_4281_0353_11D1_905F_0000_C0CC_16BA;
/// MEDIATYPE_Video ("vids").
pub const MEDIATYPE_VIDEO: u128 = media_subtype(u32::from_le_bytes(*b"vids"));
/// MERIT_DO_NOT_USE: listed as a camera, never picked by itself for something else.
pub const MERIT_DO_NOT_USE: u32 = 0x0020_0000;
/// REG_PINFLAG_B_OUTPUT.
const PIN_OUTPUT: u32 = 0x8;

/// A media subtype from its FourCC: `{FOURCC-0000-0010-8000-00AA00389B71}`.
pub const fn media_subtype(fourcc: u32) -> u128 {
    ((fourcc as u128) << 96) | 0x0000_0010_8000_00AA_0038_9B71
}

/// A registry value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Binary(Vec<u8>),
}

/// One value to write: its key (under `Software\Classes`), its name (None: the default value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub name: Option<&'static str>,
    pub value: Value,
}

fn text(key: &str, name: Option<&'static str>, v: &str) -> Entry {
    Entry { key: key.to_string(), name, value: Value::Text(v.to_string()) }
}

/// A COM class in a DLL, loadable from any thread (`ThreadingModel` Both, as DirectShow and
/// the frame server need).
fn com_class(clsid: u128, dll: &str) -> Vec<Entry> {
    let key = format!("CLSID\\{}", guid_text(clsid));
    vec![
        text(&key, None, FRIENDLY_NAME),
        text(&format!("{key}\\InprocServer32"), None, dll),
        text(&format!("{key}\\InprocServer32"), Some("ThreadingModel"), "Both"),
    ]
}

/// The DirectShow camera: its class, and its place among the video capture sources.
pub fn dshow_entries(dll: &str, formats: &[u32]) -> Vec<Entry> {
    let mut out = com_class(CLSID_DSHOW, dll);
    let inst = dshow_instance_key();
    out.push(text(&inst, Some("CLSID"), &guid_text(CLSID_DSHOW)));
    out.push(text(&inst, Some("FriendlyName"), FRIENDLY_NAME));
    out.push(Entry { key: inst, name: Some("FilterData"), value: Value::Binary(filter_data(formats)) });
    out
}

/// Where the camera is listed among DirectShow's video capture sources.
pub fn dshow_instance_key() -> String {
    format!("CLSID\\{}\\Instance\\{}", guid_text(CLSID_VIDEO_INPUT_CATEGORY), guid_text(CLSID_DSHOW))
}

/// The DirectShow camera's keys to delete (each with what's under it).
pub fn dshow_keys() -> Vec<String> {
    vec![dshow_instance_key(), format!("CLSID\\{}", guid_text(CLSID_DSHOW))]
}

/// The Media Foundation media source's class (the frame server makes it by this).
pub fn mf_entries(dll: &str) -> Vec<Entry> {
    com_class(CLSID_MF, dll)
}

/// Its key to delete.
pub fn mf_keys() -> Vec<String> {
    vec![format!("CLSID\\{}", guid_text(CLSID_MF))]
}

/// DirectShow's `FilterData`: the filter's merit and its one output pin (category capture,
/// video in each of `formats`' FourCCs), in the binary form `IFilterMapper2::RegisterFilter`
/// writes (REGFILTER2 version 2): a header, the pin, its media types, then the GUIDs they
/// point at by offset.
pub fn filter_data(formats: &[u32]) -> Vec<u8> {
    let types = formats.len() as u32;
    // Header 16, the pin 24 + its category offset 4, each type 16.
    let main = 16 + 24 + 4 + 16 * types;
    let mut guids: Vec<u128> = Vec::new();
    let mut at = |g: u128| -> u32 {
        let i = guids.iter().position(|&x| x == g).unwrap_or_else(|| {
            guids.push(g);
            guids.len() - 1
        });
        main + 16 * i as u32
    };
    let mut out = Vec::with_capacity(main as usize + 16 * (2 + formats.len()));
    let dword = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    // REG_RF: version, merit, pins, unused.
    for v in [2, MERIT_DO_NOT_USE, 1, 0] {
        dword(&mut out, v);
    }
    // REG_RFP: "0pi3", flags, instances, media types, mediums, has a category; then where it is.
    out.extend_from_slice(b"0pi3");
    for v in [PIN_OUTPUT, 1, types, 0, 1] {
        dword(&mut out, v);
    }
    let cat = at(PIN_CATEGORY_CAPTURE);
    dword(&mut out, cat);
    // REG_TYPE each: "0ty3" (the digit counts up), unused, where the major and minor types are.
    for (i, &f) in formats.iter().enumerate() {
        out.extend_from_slice(&[b'0' + i as u8, b't', b'y', b'3']);
        let (major, minor) = (at(MEDIATYPE_VIDEO), at(media_subtype(f)));
        for v in [0, major, minor] {
            dword(&mut out, v);
        }
    }
    for g in guids {
        out.extend_from_slice(&guid_bytes(g));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dword(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn subtypes_follow_the_fourcc_rule() {
        // MEDIASUBTYPE_NV12 is {3231564E-0000-0010-8000-00AA00389B71}.
        assert_eq!(guid_text(media_subtype(u32::from_le_bytes(*b"NV12"))), "{3231564E-0000-0010-8000-00AA00389B71}");
        assert_eq!(guid_text(MEDIATYPE_VIDEO), "{73646976-0000-0010-8000-00AA00389B71}");
    }

    #[test]
    fn filter_data_is_what_the_filter_mapper_writes() {
        let nv12 = u32::from_le_bytes(*b"NV12");
        let yuy2 = u32::from_le_bytes(*b"YUY2");
        let d = filter_data(&[nv12, yuy2]);
        // Header, pin + category offset, two types; then category, video, NV12, YUY2 GUIDs.
        let main = 16 + 28 + 32;
        assert_eq!(d.len(), main + 4 * 16);
        assert_eq!([dword(&d, 0), dword(&d, 4), dword(&d, 8)], [2, MERIT_DO_NOT_USE, 1]);
        assert_eq!(&d[16..20], b"0pi3");
        assert_eq!([dword(&d, 20), dword(&d, 24), dword(&d, 28), dword(&d, 32), dword(&d, 36)], [8, 1, 2, 0, 1]);
        let guid_at = |off: u32| &d[off as usize..off as usize + 16];
        assert_eq!(guid_at(dword(&d, 40)), guid_bytes(PIN_CATEGORY_CAPTURE));
        assert_eq!(&d[44..48], b"0ty3");
        assert_eq!(guid_at(dword(&d, 52)), guid_bytes(MEDIATYPE_VIDEO));
        assert_eq!(guid_at(dword(&d, 56)), guid_bytes(media_subtype(nv12)));
        assert_eq!(&d[60..64], b"1ty3");
        assert_eq!(dword(&d, 68), dword(&d, 52), "the video GUID is stored once");
        assert_eq!(guid_at(dword(&d, 72)), guid_bytes(media_subtype(yuy2)));
    }

    #[test]
    fn the_camera_is_registered_and_listed_by_name() {
        let e = dshow_entries(r"C:\Users\a\AppData\Local\Ferrocast\ferrocast_camera.dll", &[1]);
        let find = |key: &str, name: Option<&str>| e.iter().find(|x| x.key == key && x.name == name).map(|x| &x.value);
        let class = "CLSID\\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}";
        assert_eq!(find(class, None), Some(&Value::Text("Ferrocast Camera".into())));
        assert_eq!(
            find(&format!("{class}\\InprocServer32"), None),
            Some(&Value::Text(r"C:\Users\a\AppData\Local\Ferrocast\ferrocast_camera.dll".into()))
        );
        assert_eq!(
            find(&format!("{class}\\InprocServer32"), Some("ThreadingModel")),
            Some(&Value::Text("Both".into()))
        );
        let inst = "CLSID\\{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\Instance\\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}";
        assert_eq!(find(inst, Some("FriendlyName")), Some(&Value::Text("Ferrocast Camera".into())));
        assert!(matches!(find(inst, Some("FilterData")), Some(Value::Binary(_))));
        assert_eq!(dshow_keys(), [inst.to_string(), class.to_string()]);
        let mf = mf_entries(r"C:\Program Files\Ferrocast Camera\ferrocast_camera_v1.dll");
        assert_eq!(mf[0].key, "CLSID\\{42F40093-E449-4B73-9730-7D6CB5773F8A}");
        assert_eq!(mf_keys(), ["CLSID\\{42F40093-E449-4B73-9730-7D6CB5773F8A}".to_string()]);
    }
}
