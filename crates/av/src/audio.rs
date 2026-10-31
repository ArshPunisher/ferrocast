//! Audio encoding: interleaved 32-bit float sound in, AAC-LC frames out (what RTMP platforms and
//! MP4 take). macOS uses AudioToolbox's encoder (`apple/aac.rs`).

use crate::AvError;

/// Samples per AAC frame, per channel.
pub const AAC_FRAME: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// 48000 or 44100.
    pub sample_rate: u32,
    /// 1 or 2.
    pub channels: u32,
    pub bitrate_kbps: u32,
}

/// One AAC frame (raw, no ADTS header). Time in microseconds from the first sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub pts_us: i64,
    pub data: Vec<u8>,
}

/// The AudioSpecificConfig for AAC-LC (FLV's and MP4's audio header): 5 bits object type,
/// 4 bits rate index, 4 bits channels, 3 bits zero.
pub fn aac_config(sample_rate: u32, channels: u32) -> Option<[u8; 2]> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let i = RATES.iter().position(|r| *r == sample_rate)? as u16;
    if !(1..=7).contains(&channels) {
        return None;
    }
    let v: u16 = (2 << 11) | (i << 7) | ((channels as u16) << 3);
    Some(v.to_be_bytes())
}

#[derive(Debug)]
pub struct AudioEncoder {
    #[cfg(target_os = "macos")]
    inner: crate::apple::aac::Encoder,
    config: AudioConfig,
    pending: Vec<f32>,
    /// AAC frames made so far: the next one's time.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    frames: i64,
}

impl AudioEncoder {
    pub fn new(config: AudioConfig) -> Result<Self, AvError> {
        if aac_config(config.sample_rate, config.channels).is_none() || config.channels > 2 {
            return Err(AvError::Encoder(format!(
                "AAC takes 1 or 2 channels at a standard rate, not {} at {} Hz",
                config.channels, config.sample_rate
            )));
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self { inner: crate::apple::aac::Encoder::new(&config)?, config, pending: Vec::new(), frames: 0 })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AvError::Encoder("this system's AAC encoder isn't built yet".into()))
        }
    }

    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// The AudioSpecificConfig.
    pub fn header(&self) -> Vec<u8> {
        aac_config(self.config.sample_rate, self.config.channels).map(|c| c.to_vec()).unwrap_or_default()
    }

    /// Adds interleaved samples; returns every whole AAC frame now ready.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<AudioPacket>, AvError> {
        self.pending.extend_from_slice(pcm);
        let n = AAC_FRAME * self.config.channels as usize;
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut out = Vec::new();
        while self.pending.len() >= n {
            #[cfg(target_os = "macos")]
            for data in self.inner.encode(&self.pending[..n])? {
                // The encoder's output starts `priming` samples early: those are its warm-up.
                // Frames wholly inside it are dropped; times are where the real sound is.
                let at = self.frames * AAC_FRAME as i64 - i64::from(self.inner.priming);
                self.frames += 1;
                if at + (AAC_FRAME as i64) <= 0 {
                    continue;
                }
                let pts_us = at.max(0) * 1_000_000 / i64::from(self.config.sample_rate);
                out.push(AudioPacket { pts_us, data });
            }
            self.pending.drain(..n);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_for_48k_stereo() {
        assert_eq!(aac_config(48000, 2), Some([0x11, 0x90]));
        assert_eq!(aac_config(44100, 2), Some([0x12, 0x10]));
        assert_eq!(aac_config(44101, 2), None);
    }
}
