//! Ferrocast Camera on Windows, as one DLL with two COM classes:
//!
//! - a DirectShow source filter (`CLSID_DSHOW`): every DirectShow app (Zoom, Discord, OBS,
//!   browsers…) loads it into itself and lists it among its cameras; registered for the user;
//! - a Media Foundation media source (`CLSID_MF`, handed out through an activator): Windows 11's camera frame server loads it
//!   when Ferrocast makes it a virtual camera (`MFCreateVirtualCamera`), and every camera app
//!   sees it.
//!
//! Both read Ferrocast's frames from shared memory (`ferrocast-camshare`) and show a
//! placeholder while it isn't sending. See docs/plan/VIRTUAL-CAMERA-WINDOWS.md.

#[cfg(windows)]
mod activate;
#[cfg(windows)]
mod dll;
#[cfg(windows)]
mod dshow;
#[cfg(windows)]
mod media;
#[cfg(windows)]
mod mf;

#[cfg(windows)]
pub use dll::{install_machine, uninstall_machine};
