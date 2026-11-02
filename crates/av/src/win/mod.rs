//! Windows: screens, windows, cameras, microphones, the computer's sound and video files.
//! Screens and windows come through Windows' own capture (`wgc`), else FFmpeg (Desktop
//! Duplication, GDI); cameras and files through FFmpeg (`ffsrc`, DirectShow), sound through
//! cpal (WASAPI); Windows' own API lists the displays and windows.

#![allow(unsafe_code)]

pub mod camera;
pub mod screen;
pub mod wgc;

/// Video and sound files.
pub mod media {
    pub use crate::ffsrc::{Control, Media, MediaState};
}
