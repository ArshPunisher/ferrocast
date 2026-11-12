//! DirectShow media types (`AM_MEDIA_TYPE` with a `VIDEOINFOHEADER`) for the camera's modes.

use ferrocast_camshare::caps::{self, Mode};
use ferrocast_camshare::regdata::media_subtype;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::MediaFoundation::{AM_MEDIA_TYPE, FORMAT_VideoInfo, MEDIATYPE_Video, VIDEOINFOHEADER};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows::core::GUID;

pub(crate) fn guid(g: u128) -> GUID {
    GUID::from_u128(g)
}

/// The VIDEOINFOHEADER for `m`.
fn header(m: &Mode) -> VIDEOINFOHEADER {
    let len = m.frame_len() as u32;
    VIDEOINFOHEADER {
        rcSource: RECT::default(),
        rcTarget: RECT::default(),
        dwBitRate: len.saturating_mul(8).saturating_mul(m.fps().max(1)),
        dwBitErrorRate: 0,
        AvgTimePerFrame: m.interval,
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: m.width as i32,
            biHeight: m.height as i32,
            biPlanes: 1,
            biBitCount: m.format.bits(),
            biCompression: m.format.fourcc(),
            biSizeImage: len,
            ..Default::default()
        },
    }
}

/// Fills `mt` (whose format block nobody owns yet) with `m`; the block is CoTaskMemAlloc'd, as
/// DirectShow frees it.
///
/// # Safety
/// `mt` must point at writable memory for an AM_MEDIA_TYPE.
pub(crate) unsafe fn write(mt: *mut AM_MEDIA_TYPE, m: &Mode) -> windows::core::Result<()> {
    let size = std::mem::size_of::<VIDEOINFOHEADER>();
    // SAFETY: a plain allocation, checked below.
    let block = unsafe { CoTaskMemAlloc(size) }.cast::<VIDEOINFOHEADER>();
    if block.is_null() {
        return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
    }
    // SAFETY: `block` is `size` bytes, freshly allocated; `mt` is writable (the caller's word).
    unsafe {
        block.write(header(m));
        mt.write(AM_MEDIA_TYPE {
            majortype: MEDIATYPE_Video,
            subtype: guid(media_subtype(m.format.fourcc())),
            bFixedSizeSamples: true.into(),
            bTemporalCompression: false.into(),
            lSampleSize: m.frame_len() as u32,
            formattype: FORMAT_VideoInfo,
            pUnk: std::mem::ManuallyDrop::new(None),
            cbFormat: size as u32,
            pbFormat: block.cast(),
        });
    }
    Ok(())
}

/// A new AM_MEDIA_TYPE for `m`, all CoTaskMemAlloc'd (what IEnumMediaTypes and GetStreamCaps
/// hand out; the caller frees it with `DeleteMediaType`'s rules).
pub(crate) fn alloc(m: &Mode) -> windows::core::Result<*mut AM_MEDIA_TYPE> {
    // SAFETY: a plain allocation, checked below.
    let mt = unsafe { CoTaskMemAlloc(std::mem::size_of::<AM_MEDIA_TYPE>()) }.cast::<AM_MEDIA_TYPE>();
    if mt.is_null() {
        return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
    }
    // SAFETY: `mt` is ours, the size of an AM_MEDIA_TYPE.
    if let Err(e) = unsafe { write(mt, m) } {
        // SAFETY: allocated above, not handed out.
        unsafe { CoTaskMemFree(Some(mt.cast())) };
        return Err(e);
    }
    Ok(mt)
}

/// Frees what `write` put in a media type (not the media type itself).
///
/// # Safety
/// `mt` must be a media type whose format block (if any) is CoTaskMemAlloc'd.
pub(crate) unsafe fn free_format(mt: &mut AM_MEDIA_TYPE) {
    if !mt.pbFormat.is_null() {
        // SAFETY: the caller's word: CoTaskMemAlloc'd.
        unsafe { CoTaskMemFree(Some(mt.pbFormat.cast())) };
        mt.pbFormat = std::ptr::null_mut();
        mt.cbFormat = 0;
    }
    // SAFETY: drops the IUnknown the type may hold (normally none).
    unsafe { std::mem::ManuallyDrop::drop(&mut mt.pUnk) };
    mt.pUnk = std::mem::ManuallyDrop::new(None);
}

/// The mode a media type asks for, when it's video in a VIDEOINFOHEADER in one of our formats
/// and a size and rate the camera can send. A rate of 0 is taken as `fallback`'s.
///
/// # Safety
/// `mt` must be null or point at a valid AM_MEDIA_TYPE.
pub(crate) unsafe fn read(mt: *const AM_MEDIA_TYPE, fallback_interval: i64) -> Option<Mode> {
    // SAFETY: the caller's word.
    let mt = unsafe { mt.as_ref() }?;
    if mt.majortype != MEDIATYPE_Video
        || mt.formattype != FORMAT_VideoInfo
        || mt.pbFormat.is_null()
        || (mt.cbFormat as usize) < std::mem::size_of::<VIDEOINFOHEADER>()
    {
        return None;
    }
    // SAFETY: checked above: a VIDEOINFOHEADER's worth of bytes (maybe unaligned).
    let vih = unsafe { mt.pbFormat.cast::<VIDEOINFOHEADER>().read_unaligned() };
    let format = caps::format_of(vih.bmiHeader.biCompression)?;
    if guid(media_subtype(format.fourcc())) != mt.subtype {
        return None;
    }
    let m = Mode {
        width: vih.bmiHeader.biWidth.unsigned_abs(),
        height: vih.bmiHeader.biHeight.unsigned_abs(),
        format,
        interval: if vih.AvgTimePerFrame > 0 { vih.AvgTimePerFrame } else { fallback_interval },
    };
    caps::acceptable(&m).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_camshare::feed::Format;
    use windows::Win32::System::Com::CoTaskMemFree;

    fn round(m: Mode) -> Option<Mode> {
        let mt = alloc(&m).ok()?;
        // SAFETY: ours, just made; freed below.
        let back = unsafe { read(mt, 0) };
        // SAFETY: as above.
        unsafe {
            free_format(&mut *mt);
            CoTaskMemFree(Some(mt.cast()));
        }
        back
    }

    #[test]
    fn every_offered_mode_reads_back_as_itself() {
        for m in caps::offered(None) {
            assert_eq!(round(m), Some(m), "{m:?}");
        }
    }

    #[test]
    fn what_isnt_our_video_is_refused() {
        let m = Mode { width: 1280, height: 720, format: Format::Yuy2, interval: 333_333 };
        let mt = alloc(&m).unwrap_or_else(|e| panic!("{e}"));
        // SAFETY: ours, changed and read, then freed.
        unsafe {
            (*mt).subtype = guid(media_subtype(u32::from_le_bytes(*b"MJPG")));
            assert_eq!(read(mt, 0), None, "a format we don't send");
            (*mt).subtype = guid(media_subtype(Format::Nv12.fourcc()));
            assert_eq!(read(mt, 0), None, "subtype and header disagree");
            assert_eq!(read(std::ptr::null(), 0), None);
            free_format(&mut *mt);
            CoTaskMemFree(Some(mt.cast()));
        }
    }

    #[test]
    fn a_type_without_a_rate_takes_the_fallback() {
        let m = Mode { width: 640, height: 360, format: Format::Nv12, interval: 333_333 };
        let mt = alloc(&m).unwrap_or_else(|e| panic!("{e}"));
        // SAFETY: ours, changed and read, then freed.
        unsafe {
            (*(*mt).pbFormat.cast::<VIDEOINFOHEADER>()).AvgTimePerFrame = 0;
            assert_eq!(read(mt, 166_666).map(|x| x.interval), Some(166_666));
            free_format(&mut *mt);
            CoTaskMemFree(Some(mt.cast()));
        }
    }
}
