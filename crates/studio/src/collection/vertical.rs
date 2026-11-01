//! The vertical picture: a second canvas, tall, with scenes of its own. Sources are shared with
//! the main picture (one capture, placed differently in each); scenes aren't. It goes out on its
//! own, so phones get a picture made for them instead of one cut from the wide stream.

use serde::Serialize;

use super::SceneInfo;

/// The vertical picture's size, as Shorts, TikTok and Reels want it.
pub const VERTICAL_SIZE: (u32, u32) = (1080, 1920);

/// How the vertical picture stands.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VerticalInfo {
    pub width: u32,
    pub height: u32,
    pub scenes: Vec<SceneInfo>,
    pub current: String,
}
