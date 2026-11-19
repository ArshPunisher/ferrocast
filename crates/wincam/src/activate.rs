//! What COM hands out for the Media Foundation camera's class: an activator (`IMFActivate`),
//! not the media source itself. Windows' camera frame server creates the class, asks it for
//! `IMFActivate` (anything else fails with E_NOINTERFACE, so the camera never starts) and calls
//! `ActivateObject` for the source. The attributes it sets on the activator first are handed
//! to the source.
//!
//! An activator is an attribute store too: every `IMFAttributes` call goes to a store of
//! Media Foundation's own, through its vtable, so values and errors are exactly its.

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, IMFAttributes_Impl, IMFMediaSource, MF_ATTRIBUTE_TYPE,
    MF_ATTRIBUTES_MATCH_TYPE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{BOOL, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

#[implement(IMFActivate)]
pub(crate) struct Activate {
    attrs: IMFAttributes,
    /// The source made by `ActivateObject`, handed out again until shut down or detached.
    source: Mutex<Option<IUnknown>>,
}

impl Activate {
    pub(crate) fn create() -> windows::core::Result<IUnknown> {
        let a: IMFActivate = Activate { attrs: crate::mf::new_attributes(4)?, source: Mutex::new(None) }.into();
        a.cast()
    }

    fn lock(&self) -> MutexGuard<'_, Option<IUnknown>> {
        self.source.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Raw pointer of an optional interface argument (null when absent).
fn raw_of<T: Interface>(r: &Ref<'_, T>) -> *mut c_void {
    r.as_ref().map_or(std::ptr::null_mut(), Interface::as_raw)
}

/// Calls `IMFAttributes::$m` of the inner store with these arguments → its HRESULT.
macro_rules! inner {
    ($self:ident.$m:ident($($a:expr),*)) => {{
        let a = &$self.attrs;
        // SAFETY: the inner store's own vtable entry, with the caller's arguments passed on
        // unchanged (the same ABI types this method received).
        unsafe { (Interface::vtable(a).$m)(a.as_raw() $(, $a)*) }
    }};
}

impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> windows::core::Result<()> {
        inner!(self.GetItem(key, value)).ok()
    }

    fn GetItemType(&self, key: *const GUID) -> windows::core::Result<MF_ATTRIBUTE_TYPE> {
        let mut out = MF_ATTRIBUTE_TYPE::default();
        inner!(self.GetItemType(key, &mut out)).map(|| out)
    }

    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows::core::Result<BOOL> {
        let mut out = BOOL::default();
        inner!(self.CompareItem(key, value, &mut out)).map(|| out)
    }

    fn Compare(&self, theirs: Ref<'_, IMFAttributes>, kind: MF_ATTRIBUTES_MATCH_TYPE) -> windows::core::Result<BOOL> {
        let mut out = BOOL::default();
        inner!(self.Compare(raw_of(&theirs), kind, &mut out)).map(|| out)
    }

    fn GetUINT32(&self, key: *const GUID) -> windows::core::Result<u32> {
        let mut out = 0;
        inner!(self.GetUINT32(key, &mut out)).map(|| out)
    }

    fn GetUINT64(&self, key: *const GUID) -> windows::core::Result<u64> {
        let mut out = 0;
        inner!(self.GetUINT64(key, &mut out)).map(|| out)
    }

    fn GetDouble(&self, key: *const GUID) -> windows::core::Result<f64> {
        let mut out = 0.0;
        inner!(self.GetDouble(key, &mut out)).map(|| out)
    }

    fn GetGUID(&self, key: *const GUID) -> windows::core::Result<GUID> {
        let mut out = GUID::zeroed();
        inner!(self.GetGUID(key, &mut out)).map(|| out)
    }

    fn GetStringLength(&self, key: *const GUID) -> windows::core::Result<u32> {
        let mut out = 0;
        inner!(self.GetStringLength(key, &mut out)).map(|| out)
    }

    fn GetString(&self, key: *const GUID, buf: PWSTR, size: u32, len: *mut u32) -> windows::core::Result<()> {
        inner!(self.GetString(key, buf, size, len)).ok()
    }

    fn GetAllocatedString(&self, key: *const GUID, out: *mut PWSTR, len: *mut u32) -> windows::core::Result<()> {
        inner!(self.GetAllocatedString(key, out, len)).ok()
    }

    fn GetBlobSize(&self, key: *const GUID) -> windows::core::Result<u32> {
        let mut out = 0;
        inner!(self.GetBlobSize(key, &mut out)).map(|| out)
    }

    fn GetBlob(&self, key: *const GUID, buf: *mut u8, size: u32, len: *mut u32) -> windows::core::Result<()> {
        inner!(self.GetBlob(key, buf, size, len)).ok()
    }

    fn GetAllocatedBlob(&self, key: *const GUID, out: *mut *mut u8, len: *mut u32) -> windows::core::Result<()> {
        inner!(self.GetAllocatedBlob(key, out, len)).ok()
    }

    fn GetUnknown(&self, key: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> windows::core::Result<()> {
        inner!(self.GetUnknown(key, iid, out)).ok()
    }

    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows::core::Result<()> {
        inner!(self.SetItem(key, value)).ok()
    }

    fn DeleteItem(&self, key: *const GUID) -> windows::core::Result<()> {
        inner!(self.DeleteItem(key)).ok()
    }

    fn DeleteAllItems(&self) -> windows::core::Result<()> {
        inner!(self.DeleteAllItems()).ok()
    }

    fn SetUINT32(&self, key: *const GUID, value: u32) -> windows::core::Result<()> {
        inner!(self.SetUINT32(key, value)).ok()
    }

    fn SetUINT64(&self, key: *const GUID, value: u64) -> windows::core::Result<()> {
        inner!(self.SetUINT64(key, value)).ok()
    }

    fn SetDouble(&self, key: *const GUID, value: f64) -> windows::core::Result<()> {
        inner!(self.SetDouble(key, value)).ok()
    }

    fn SetGUID(&self, key: *const GUID, value: *const GUID) -> windows::core::Result<()> {
        inner!(self.SetGUID(key, value)).ok()
    }

    fn SetString(&self, key: *const GUID, value: &PCWSTR) -> windows::core::Result<()> {
        inner!(self.SetString(key, *value)).ok()
    }

    fn SetBlob(&self, key: *const GUID, buf: *const u8, size: u32) -> windows::core::Result<()> {
        inner!(self.SetBlob(key, buf, size)).ok()
    }

    fn SetUnknown(&self, key: *const GUID, value: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        inner!(self.SetUnknown(key, raw_of(&value))).ok()
    }

    fn LockStore(&self) -> windows::core::Result<()> {
        inner!(self.LockStore()).ok()
    }

    fn UnlockStore(&self) -> windows::core::Result<()> {
        inner!(self.UnlockStore()).ok()
    }

    fn GetCount(&self) -> windows::core::Result<u32> {
        let mut out = 0;
        inner!(self.GetCount(&mut out)).map(|| out)
    }

    fn GetItemByIndex(&self, index: u32, key: *mut GUID, value: *mut PROPVARIANT) -> windows::core::Result<()> {
        inner!(self.GetItemByIndex(index, key, value)).ok()
    }

    fn CopyAllItems(&self, dest: Ref<'_, IMFAttributes>) -> windows::core::Result<()> {
        inner!(self.CopyAllItems(raw_of(&dest))).ok()
    }
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(&self, iid: *const GUID, out: *mut *mut c_void) -> windows::core::Result<()> {
        if iid.is_null() || out.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the caller's out-pointer, cleared first as COM asks.
        unsafe { out.write(std::ptr::null_mut()) };
        let mut held = self.lock();
        let source = match held.as_ref() {
            Some(s) => s.clone(),
            None => {
                let s = crate::mf::Source::create(Some(&self.attrs))?;
                *held = Some(s.clone());
                s
            }
        };
        // SAFETY: hands the caller the interface it asked for (counted), or E_NOINTERFACE.
        unsafe { source.query(iid, out) }.ok()
    }

    fn ShutdownObject(&self) -> windows::core::Result<()> {
        if let Some(s) = self.lock().take()
            && let Ok(source) = s.cast::<IMFMediaSource>()
        {
            // SAFETY: shuts down the source we made.
            let _ = unsafe { source.Shutdown() };
        }
        Ok(())
    }

    fn DetachObject(&self) -> windows::core::Result<()> {
        // The caller owns the source now: let go of it without shutting it down.
        self.lock().take();
        Ok(())
    }
}
