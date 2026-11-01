//! Windows: screens, windows, cameras, microphones, the computer's sound and video files.
//! Screens, cameras and files are read through FFmpeg (`ffsrc`: Desktop Duplication, GDI and
//! DirectShow), sound through cpal (WASAPI); Windows' own API lists the displays and windows.

#![allow(unsafe_code)]

pub mod camera;
pub mod screen;

/// Video and sound files.
pub mod media {
    pub use crate::ffsrc::{Control, Media, MediaState};
}
