//! How loud OBS hears an audio source, for music pieces that move with the beat.
//!
//! OBS sends its volume meters about 20 times a second to a connection that asks for them
//! (`InputVolumeMeters`, a high-volume event). That is a second connection of its own, so the
//! main one never carries the flood.

use std::time::Duration;

use futures_util::StreamExt;
use obws::client::ConnectConfig;
use obws::events::Event;
use obws::requests::EventSubscription;

use crate::{Client, ObsConfig, ObsError, map_err};

/// A connection that receives OBS's volume meters, and its stream of readings.
#[allow(missing_debug_implementations)]
pub struct Meters {
    /// Kept open for as long as readings are wanted.
    _client: Client,
    stream: obws::events::EventStream,
}

/// Opens the meters connection.
pub async fn connect(cfg: &ObsConfig, password: Option<&str>) -> Result<Meters, ObsError> {
    let config = ConnectConfig {
        host: cfg.host.as_str(),
        port: cfg.port,
        dangerous: None,
        password,
        event_subscriptions: Some(EventSubscription::INPUT_VOLUME_METERS),
        broadcast_capacity: 256,
        connect_timeout: Duration::from_secs(4),
    };
    let client = match tokio::time::timeout(Duration::from_secs(5), Client::connect_with_config(config)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(map_err(e)),
        Err(_) => return Err(ObsError::NotReachable),
    };
    let stream = client.events().map_err(map_err)?;
    Ok(Meters { _client: client, stream })
}

impl Meters {
    /// The next reading for `input`: its loudest channel's peak, 0 to 1 (linear). `Some(0.0)`
    /// when OBS sent meters without it (muted or silent); `None` when the connection closed.
    pub async fn next(&mut self, input: &str) -> Option<f32> {
        loop {
            match self.stream.next().await? {
                Event::InputVolumeMeters { inputs } => {
                    let peak = inputs
                        .iter()
                        .find(|i| i.name == input)
                        .map(|i| i.levels.iter().map(|ch| ch[1]).fold(0.0_f32, f32::max))
                        .unwrap_or(0.0);
                    return Some(if peak.is_finite() { peak.clamp(0.0, 1.0) } else { 0.0 });
                }
                _ => continue,
            }
        }
    }
}

/// A peak (linear, 0 to 1) as how loud it looks: -50 dB and below is 0, 0 dB is 1.
pub fn loudness(peak: f32) -> f32 {
    if peak <= 0.0 || !peak.is_finite() {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db + 50.0) / 50.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::loudness;

    #[test]
    fn loudness_follows_decibels() {
        assert_eq!(loudness(0.0), 0.0);
        assert_eq!(loudness(f32::NAN), 0.0);
        assert!((loudness(1.0) - 1.0).abs() < 1e-6);
        // -20 dB is a normal song in OBS's meter: a bit over half.
        assert!((loudness(0.1) - 0.6).abs() < 1e-3);
        assert_eq!(loudness(0.000_1), 0.0, "-80 dB is silence");
    }
}
